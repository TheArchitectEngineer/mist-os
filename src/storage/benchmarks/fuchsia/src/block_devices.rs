// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fidl::endpoints::{
    create_proxy, create_request_stream, DiscoverableProtocolMarker as _, Proxy,
};
use fidl::HandleBased as _;
use fidl_fuchsia_device::ControllerMarker;
use fidl_fuchsia_fs_startup::{CreateOptions, MountOptions};
use fidl_fuchsia_hardware_block::BlockMarker;
use fidl_fuchsia_hardware_block_volume::{
    VolumeManagerMarker, VolumeManagerProxy, VolumeMarker, VolumeSynchronousProxy,
    ALLOCATE_PARTITION_FLAG_INACTIVE,
};
use fidl_fuchsia_io as fio;
use fs_management::filesystem::{
    BlockConnector, DirBasedBlockConnector, ServingMultiVolumeFilesystem,
};
use fs_management::format::DiskFormat;
use fs_management::{Fvm, BLOBFS_TYPE_GUID};
use fuchsia_component::client::{
    connect_to_named_protocol_at_dir_root, connect_to_protocol, connect_to_protocol_at_dir_root,
    connect_to_protocol_at_path, Service,
};

use std::path::PathBuf;
use std::sync::Arc;
use storage_benchmarks::block_device::BlockDevice;
use storage_benchmarks::{BlockDeviceConfig, BlockDeviceFactory};
use storage_isolated_driver_manager::{
    create_random_guid, find_block_device, find_block_device_devfs, fvm, into_guid,
    wait_for_block_device_devfs, zxcrypt, BlockDeviceMatcher, Guid,
};
use {
    fidl_fuchsia_device as fdevice, fidl_fuchsia_storage_partitions as fpartitions,
    fuchsia_async as fasync,
};

const BLOBFS_VOLUME_NAME: &str = "blobfs";

const BENCHMARK_FVM_SIZE_BYTES: u64 = 160 * 1024 * 1024;
// 8MiB is the default slice size; use it so the test FVM partition matches the performance of the
// system FVM partition (so they are interchangeable).
// Note that this only affects the performance of minfs and blobfs, since these two filesystems are
// the only ones that dynamically allocate from FVM.
const BENCHMARK_FVM_SLICE_SIZE_BYTES: usize = 8 * 1024 * 1024;

// On systems which don't have FVM (i.e. Fxblob), we create an FVM partition the test can use, with
// this GUID.  See connect_to_test_fvm for details.

const BENCHMARK_FVM_TYPE_GUID: &Guid = &[
    0x67, 0x45, 0x23, 0x01, 0xab, 0x89, 0xef, 0xcd, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88,
];
const BENCHMARK_FVM_VOLUME_NAME: &str = "benchmark-fvm";

const BENCHMARK_TYPE_GUID: &Guid = &[
    0x67, 0x45, 0x23, 0x01, 0xab, 0x89, 0xef, 0xcd, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];
const BENCHMARK_VOLUME_NAME: &str = "benchmark";

/// Returns the exposed directory of the volume, as well as the task running Crypt for the volume
/// (if configured in `config`).
pub async fn create_fvm_volume(
    fvm: &fidl_fuchsia_fs_startup::VolumesProxy,
    instance_guid: [u8; 16],
    config: &BlockDeviceConfig,
) -> (fio::DirectoryProxy, Option<fasync::Task<()>>) {
    let (crypt, crypt_task) = if config.use_zxcrypt {
        let (crypt, stream) = create_request_stream::<fidl_fuchsia_fxfs::CryptMarker>();
        let task = fasync::Task::spawn(async {
            if let Err(err) =
                zxcrypt_crypt::run_crypt_service(crypt_policy::Policy::Null, stream).await
            {
                log::error!(err:?; "Crypt service failure");
            }
        });
        (Some(crypt), Some(task))
    } else {
        (None, None)
    };
    let (volume_dir, server_end) = create_proxy::<fio::DirectoryMarker>();
    fvm.create(
        BENCHMARK_VOLUME_NAME,
        server_end,
        CreateOptions {
            initial_size: config.volume_size,
            type_guid: Some(BENCHMARK_TYPE_GUID.clone()),
            guid: Some(instance_guid),
            ..Default::default()
        },
        MountOptions { crypt, ..Default::default() },
    )
    .await
    .expect("FIDL error")
    .map_err(zx::Status::from_raw)
    .expect("Failed to create volume");

    (volume_dir, crypt_task)
}

pub enum SystemFvm {
    /// The FVM is running in devfs.
    Devfs(VolumeManagerProxy),
    /// The FVM is running as a component.
    Component(
        Box<dyn Send + Sync + Fn() -> fidl_fuchsia_fs_startup::VolumesProxy>,
        Box<dyn Send + Sync + Fn() -> fio::DirectoryProxy>,
    ),
}

pub enum SystemGpt {
    /// The GPT is running in devfs.
    Devfs(fdevice::ControllerProxy),
    /// The GPT is running as a component.
    Component(Arc<fpartitions::PartitionServiceProxy>),
}

/// A factory for volumes which the benchmarks run in.  If the system has an FVM, benchmarks run in
/// a volume in the system FVM.  Otherwise, they run out of a partition in the system GPT (and might
/// still include a hermetic FVM instance, if the benchmark needs it to run).
pub enum BenchmarkVolumeFactory {
    SystemFvm(SystemFvm),
    SystemGpt(SystemGpt),
}

struct RawBlockDeviceInDevfs(fdevice::ControllerProxy);

impl BlockDevice for RawBlockDeviceInDevfs {
    fn connector(&self) -> Box<dyn BlockConnector> {
        Box::new(self.0.clone())
    }
}

struct RawBlockDeviceInGpt(Arc<fpartitions::PartitionServiceProxy>);

impl BlockDevice for RawBlockDeviceInGpt {
    fn connector(&self) -> Box<dyn BlockConnector> {
        Box::new(self.0.clone())
    }
}

#[async_trait]
impl BlockDeviceFactory for BenchmarkVolumeFactory {
    async fn create_block_device(&self, config: &BlockDeviceConfig) -> Box<dyn BlockDevice> {
        let instance_guid = create_random_guid();
        match self {
            Self::SystemFvm(SystemFvm::Devfs(volume_manager)) => Box::new(
                Self::create_fvm_volume_in_devfs(volume_manager, instance_guid, config).await,
            ),
            Self::SystemFvm(SystemFvm::Component(volumes_connector, _)) => {
                let volumes = volumes_connector();
                Box::new(Self::create_fvm_volume(volumes, instance_guid, config).await)
            }
            Self::SystemGpt(SystemGpt::Devfs(controller)) => {
                if config.requires_fvm {
                    Box::new(
                        Self::create_fvm_instance_and_volume_in_devfs(
                            controller,
                            instance_guid,
                            config,
                        )
                        .await,
                    )
                } else {
                    Box::new(RawBlockDeviceInDevfs(controller.clone()))
                }
            }
            Self::SystemGpt(SystemGpt::Component(partition_service)) => {
                if config.requires_fvm {
                    Box::new(
                        Self::create_fvm_instance_and_volume(
                            partition_service.clone(),
                            instance_guid,
                            config,
                        )
                        .await,
                    )
                } else {
                    Box::new(RawBlockDeviceInGpt(partition_service.clone()))
                }
            }
        }
    }
}

impl BenchmarkVolumeFactory {
    /// Creates a factory for volumes in which benchmarks should run in based on the provided
    /// configuration.  Uses various capabilities in the incoming namespace of the process.
    pub async fn from_config(storage_host: bool, fxfs_blob: bool) -> BenchmarkVolumeFactory {
        if storage_host {
            let partitions = Service::open(fpartitions::PartitionServiceMarker).unwrap();
            let manager = connect_to_protocol::<fpartitions::PartitionsManagerMarker>().unwrap();
            if fxfs_blob {
                let instance =
                    BenchmarkVolumeFactory::connect_to_test_partition(partitions, manager).await;
                assert!(
                    instance.is_some(),
                    "Failed to open or create testing FVM in GPT.  \
                    Perhaps the system doesn't have a GPT-formatted block device?"
                );
                instance.unwrap()
            } else {
                let volumes_connector = Box::new(move || {
                    connect_to_protocol::<fidl_fuchsia_fs_startup::VolumesMarker>().unwrap()
                });
                let volumes_dir_connector = {
                    Box::new(move || {
                        fuchsia_fs::directory::open_in_namespace("volumes", fio::PERM_READABLE)
                            .unwrap()
                    })
                };
                BenchmarkVolumeFactory::connect_to_system_fvm(
                    volumes_connector,
                    volumes_dir_connector,
                )
                .unwrap()
            }
        } else {
            if fxfs_blob {
                let instance = BenchmarkVolumeFactory::connect_to_test_partition_devfs().await;
                assert!(
                    instance.is_some(),
                    "Failed to open or create testing volume in GPT.  \
                    Perhaps the system doesn't have a GPT-formatted block device?"
                );
                instance.unwrap()
            } else {
                let instance = BenchmarkVolumeFactory::connect_to_system_fvm_devfs().await;
                assert!(
                    instance.is_some(),
                    "Failed to open or create volume in FVM.  \
                    Perhaps the system doesn't have an FVM-formatted block device?"
                );
                instance.unwrap()
            }
        }
    }

    /// Connects to the system FVM component.
    pub fn connect_to_system_fvm(
        volumes_connector: Box<dyn Send + Sync + Fn() -> fidl_fuchsia_fs_startup::VolumesProxy>,
        volumes_dir_connector: Box<dyn Send + Sync + Fn() -> fio::DirectoryProxy>,
    ) -> Option<BenchmarkVolumeFactory> {
        Some(BenchmarkVolumeFactory::SystemFvm(SystemFvm::Component(
            volumes_connector,
            volumes_dir_connector,
        )))
    }

    /// Connects to the system FVM running in devfs.
    pub async fn connect_to_system_fvm_devfs() -> Option<BenchmarkVolumeFactory> {
        // The FVM won't always have a label or GUID we can search for (e.g. on Astro where it is
        // the top-level partition exposed by the FTL).  Search for blobfs and work backwards.
        let blobfs_dev_path = find_block_device_devfs(&[
            BlockDeviceMatcher::Name(BLOBFS_VOLUME_NAME),
            BlockDeviceMatcher::TypeGuid(&BLOBFS_TYPE_GUID),
        ])
        .await
        .ok()?;

        let controller_path = format!("{}/device_controller", blobfs_dev_path.to_str().unwrap());
        let blobfs_controller = connect_to_protocol_at_path::<ControllerMarker>(&controller_path)
            .unwrap_or_else(|_| panic!("Failed to connect to Controller at {:?}", controller_path));
        let path = blobfs_controller
            .get_topological_path()
            .await
            .expect("FIDL error")
            .expect("get_topological_path failed");

        let mut path = PathBuf::from(path);
        if !path.pop() || !path.pop() {
            panic!("Unexpected topological path for Blobfs {}", path.display());
        }

        match path.file_name() {
            Some(p) => assert!(p == "fvm", "Unexpected FVM path: {}", path.display()),
            None => panic!("Unexpected FVM path: {}", path.display()),
        }
        Some(BenchmarkVolumeFactory::SystemFvm(SystemFvm::Devfs(
            connect_to_protocol_at_path::<VolumeManagerMarker>(path.to_str().unwrap())
                .unwrap_or_else(|_| panic!("Failed to connect to VolumeManager at {:?}", path)),
        )))
    }

    // Creates and connects to the partition reserved for benchmarks, or adds it to the GPT if
    // absent.  The partition will be unformatted and should be reformatted explicitly before being
    // used for a benchmark.
    pub async fn connect_to_test_partition(
        service: Service<fpartitions::PartitionServiceMarker>,
        manager: fpartitions::PartitionsManagerProxy,
    ) -> Option<BenchmarkVolumeFactory> {
        let service_instances =
            service.clone().enumerate().await.expect("Failed to enumerate partitions");
        let connector = if let Some(connector) = find_block_device(
            &[
                BlockDeviceMatcher::Name(BENCHMARK_FVM_VOLUME_NAME),
                BlockDeviceMatcher::TypeGuid(&BENCHMARK_FVM_TYPE_GUID),
            ],
            service_instances.into_iter(),
        )
        .await
        .expect("Failed to find block device")
        {
            // If the test FVM already exists, just use it.
            connector
        } else {
            // Otherwise, create it in the GPT.
            let info =
                manager.get_block_info().await.expect("FIDL error").expect("get_block_info failed");
            let transaction = manager
                .create_transaction()
                .await
                .expect("FIDL error")
                .map_err(zx::Status::from_raw)
                .expect("create_transaction failed");
            let request = fpartitions::PartitionsManagerAddPartitionRequest {
                transaction: Some(transaction.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                name: Some(BENCHMARK_FVM_VOLUME_NAME.to_string()),
                type_guid: Some(fidl_fuchsia_hardware_block_partition::Guid {
                    value: BENCHMARK_FVM_TYPE_GUID.clone(),
                }),
                num_blocks: Some(BENCHMARK_FVM_SIZE_BYTES / info.1 as u64),
                ..Default::default()
            };
            manager
                .add_partition(request)
                .await
                .expect("FIDL error")
                .map_err(zx::Status::from_raw)
                .expect("add_partition failed");
            manager
                .commit_transaction(transaction)
                .await
                .expect("FIDL error")
                .map_err(zx::Status::from_raw)
                .expect("add_partition failed");
            let service_instances =
                service.enumerate().await.expect("Failed to enumerate partitions");
            log::info!("len {}", service_instances.len());
            find_block_device(
                &[
                    BlockDeviceMatcher::Name(BENCHMARK_FVM_VOLUME_NAME),
                    BlockDeviceMatcher::TypeGuid(&BENCHMARK_FVM_TYPE_GUID),
                ],
                service_instances.into_iter(),
            )
            .await
            .expect("Failed to find block device")?
        };

        Some(BenchmarkVolumeFactory::SystemGpt(SystemGpt::Component(Arc::new(connector))))
    }

    // Creates and connects to the partition reserved for benchmarks, or adds it to the GPT if
    // absent.  The partition will be unformatted and should be reformatted explicitly before being
    // used for a benchmark.
    // TODO(https://fxbug.dev/372555079): Remove.
    pub async fn connect_to_test_partition_devfs() -> Option<BenchmarkVolumeFactory> {
        let mut fvm_path = if let Ok(path) = find_block_device_devfs(&[
            BlockDeviceMatcher::Name(BENCHMARK_FVM_VOLUME_NAME),
            BlockDeviceMatcher::TypeGuid(&BENCHMARK_FVM_TYPE_GUID),
        ])
        .await
        {
            // If the test FVM already exists, just use it.
            path
        } else {
            // Otherwise, create it in the GPT.
            let mut gpt_block_path =
                find_block_device_devfs(&[BlockDeviceMatcher::ContentsMatch(DiskFormat::Gpt)])
                    .await
                    .ok()?;
            gpt_block_path.push("device_controller");
            let gpt_block_controller =
                connect_to_protocol_at_path::<ControllerMarker>(gpt_block_path.to_str().unwrap())
                    .expect("Failed to connect to GPT controller");

            let mut gpt_path = gpt_block_controller
                .get_topological_path()
                .await
                .expect("FIDL error")
                .expect("get_topological_path failed");
            gpt_path.push_str("/gpt/device_controller");

            let gpt_controller = connect_to_protocol_at_path::<ControllerMarker>(&gpt_path)
                .expect("Failed to connect to GPT controller");

            let (volume_manager, server) = create_proxy::<VolumeManagerMarker>();
            gpt_controller
                .connect_to_device_fidl(server.into_channel())
                .expect("Failed to connect to device FIDL");
            let slice_size = {
                let (status, info) = volume_manager.get_info().await.expect("FIDL error");
                zx::ok(status).expect("Failed to get VolumeManager info");
                info.unwrap().slice_size
            };
            let slice_count = BENCHMARK_FVM_SIZE_BYTES / slice_size;
            let instance_guid = into_guid(create_random_guid());
            let status = volume_manager
                .allocate_partition(
                    slice_count,
                    &into_guid(BENCHMARK_FVM_TYPE_GUID.clone()),
                    &instance_guid,
                    BENCHMARK_FVM_VOLUME_NAME,
                    0,
                )
                .await
                .expect("FIDL error");
            zx::ok(status).expect("Failed to allocate benchmark FVM");

            wait_for_block_device_devfs(&[
                BlockDeviceMatcher::Name(BENCHMARK_FVM_VOLUME_NAME),
                BlockDeviceMatcher::TypeGuid(&BENCHMARK_FVM_TYPE_GUID),
            ])
            .await
            .expect("Failed to wait for newly created benchmark FVM to appear")
        };
        fvm_path.push("device_controller");
        let fvm_controller =
            connect_to_protocol_at_path::<ControllerMarker>(fvm_path.to_str().unwrap())
                .expect("failed to connect to controller");

        Some(BenchmarkVolumeFactory::SystemGpt(SystemGpt::Devfs(fvm_controller)))
    }

    #[cfg(test)]
    pub async fn contains_fvm_volume(&self, name: &str) -> bool {
        match self {
            Self::SystemFvm(SystemFvm::Devfs(_)) => {
                find_block_device_devfs(&[BlockDeviceMatcher::Name(name)]).await.is_ok()
            }
            Self::SystemFvm(SystemFvm::Component(_, volumes_dir_connector)) => {
                let dir = volumes_dir_connector();
                fuchsia_fs::directory::dir_contains(&dir, name).await.unwrap()
            }
            // If we're using a system GPT, the FVM instance is created on the fly, so volumes are
            // too.
            _ => false,
        }
    }

    async fn create_fvm_volume_in_devfs(
        volume_manager: &VolumeManagerProxy,
        instance_guid: [u8; 16],
        config: &BlockDeviceConfig,
    ) -> FvmVolume {
        fvm::create_fvm_volume(
            volume_manager,
            BENCHMARK_VOLUME_NAME,
            BENCHMARK_TYPE_GUID,
            &instance_guid,
            config.volume_size,
            ALLOCATE_PARTITION_FLAG_INACTIVE,
        )
        .await
        .expect("Failed to create FVM volume");

        let device_path = wait_for_block_device_devfs(&[
            BlockDeviceMatcher::TypeGuid(BENCHMARK_TYPE_GUID),
            BlockDeviceMatcher::InstanceGuid(&instance_guid),
            BlockDeviceMatcher::Name(BENCHMARK_VOLUME_NAME),
        ])
        .await
        .expect("Failed to find the FVM volume");

        // We need to connect to the volume's DirectoryProxy via its topological path in order to
        // allow the caller to access its zxcrypt child. Hence, we use the controller to get access
        // to the topological path and then call open().
        // Connect to the controller and get the device's topological path.
        let controller = connect_to_protocol_at_path::<ControllerMarker>(&format!(
            "{}/device_controller",
            device_path.to_str().unwrap()
        ))
        .expect("failed to connect to controller");
        let topo_path = controller
            .get_topological_path()
            .await
            .expect("transport error on get_topological_path")
            .expect("get_topological_path failed");
        let volume_dir =
            fuchsia_fs::directory::open_in_namespace(&topo_path, fuchsia_fs::Flags::empty())
                .expect("failed to open device");
        // TODO(https://fxbug.dev/42063787): In order to allow multiplexing to be removed, use
        // connect_to_device_fidl to connect to the BlockProxy instead of connect_to_.._dir_root.
        // Requires downstream work, i.e. set_up_fvm_volume() and set_up_insecure_zxcrypt should
        // return controllers.
        let block = connect_to_named_protocol_at_dir_root::<BlockMarker>(&volume_dir, ".").unwrap();
        let volume = VolumeSynchronousProxy::new(block.into_channel().unwrap().into());
        let volume_dir = if config.use_zxcrypt {
            zxcrypt::set_up_insecure_zxcrypt(&volume_dir).await.expect("Failed to set up zxcrypt")
        } else {
            volume_dir
        };

        FvmVolume {
            destroy_fn: Some(Box::new(move || {
                zx::ok(volume.destroy(zx::MonotonicInstant::INFINITE).unwrap())
            })),
            fvm_instance: None,
            volume_dir: Some(volume_dir),
            block_path: ".".to_string(),
            crypt_task: None,
        }
    }

    async fn create_fvm_volume(
        volumes: fidl_fuchsia_fs_startup::VolumesProxy,
        instance_guid: [u8; 16],
        config: &BlockDeviceConfig,
    ) -> FvmVolume {
        let (volume_dir, crypt_task) = create_fvm_volume(&volumes, instance_guid, config).await;
        let volumes = volumes.into_client_end().unwrap().into_sync_proxy();
        FvmVolume {
            destroy_fn: Some(Box::new(move || {
                volumes
                    .remove(BENCHMARK_VOLUME_NAME, zx::MonotonicInstant::INFINITE)
                    .unwrap()
                    .map_err(zx::Status::from_raw)
            })),
            volume_dir: Some(volume_dir),
            fvm_instance: None,
            block_path: format!("svc/{}", VolumeMarker::PROTOCOL_NAME),
            crypt_task,
        }
    }

    async fn create_fvm_instance_and_volume(
        partition: Arc<fpartitions::PartitionServiceProxy>,
        instance_guid: [u8; 16],
        config: &BlockDeviceConfig,
    ) -> FvmVolume {
        let block_device =
            partition.connect_block().expect("Failed to connect to block").into_proxy();
        fvm::format_for_fvm(&block_device, BENCHMARK_FVM_SLICE_SIZE_BYTES)
            .expect("Failed to format FVM");

        let mut fs = fs_management::filesystem::Filesystem::from_boxed_config(
            Box::new(partition),
            Box::new(Fvm::default()),
        );
        let fvm_instance = fs.serve_multi_volume().await.expect("Failed to serve FVM");
        let volumes = connect_to_protocol_at_dir_root::<fidl_fuchsia_fs_startup::VolumesMarker>(
            fvm_instance.exposed_dir(),
        )
        .unwrap();

        let (volume_dir, crypt_task) = create_fvm_volume(&volumes, instance_guid, config).await;
        FvmVolume {
            destroy_fn: None,
            volume_dir: Some(volume_dir),
            fvm_instance: Some(fvm_instance),
            block_path: format!("svc/{}", VolumeMarker::PROTOCOL_NAME),
            crypt_task,
        }
    }

    async fn create_fvm_instance_and_volume_in_devfs(
        fvm_controller: &fdevice::ControllerProxy,
        instance_guid: [u8; 16],
        config: &BlockDeviceConfig,
    ) -> FvmVolume {
        // Unbind in case anything was using the partition.
        fvm_controller
            .unbind_children()
            .await
            .expect("FIDL error")
            .expect("failed to unbind children");

        let (block_device, server_end) = create_proxy::<BlockMarker>();
        fvm_controller.connect_to_device_fidl(server_end.into_channel()).unwrap();
        fvm::format_for_fvm(&block_device, BENCHMARK_FVM_SLICE_SIZE_BYTES)
            .expect("Failed to format FVM");

        let topo_path = fvm_controller
            .get_topological_path()
            .await
            .expect("transport error on get_topological_path")
            .expect("get_topological_path failed");
        let dir = fuchsia_fs::directory::open_in_namespace(&topo_path, fio::PERM_READABLE).unwrap();
        let volume_manager =
            fvm::start_fvm_driver(&fvm_controller, &dir).await.expect("Failed to start FVM");

        Self::create_fvm_volume_in_devfs(&volume_manager, instance_guid, config).await
    }
}

/// A block device created on top of an FVM instance.
pub struct FvmVolume {
    destroy_fn: Option<Box<dyn Send + Sync + FnOnce() -> Result<(), zx::Status>>>,
    fvm_instance: Option<ServingMultiVolumeFilesystem>,
    volume_dir: Option<fio::DirectoryProxy>,
    crypt_task: Option<fasync::Task<()>>,
    // The path in `volume_dir` to connect to when opening a new Block connection.
    block_path: String,
}

impl BlockDevice for FvmVolume {
    fn connector(&self) -> Box<dyn BlockConnector> {
        let volume_dir = fuchsia_fs::directory::clone(self.volume_dir.as_ref().unwrap()).unwrap();
        Box::new(DirBasedBlockConnector::new(volume_dir, self.block_path.clone()))
    }
}

impl Drop for FvmVolume {
    fn drop(&mut self) {
        self.volume_dir = None;
        self.fvm_instance = None;
        self.crypt_task = None;
        if let Some(destroy_fn) = self.destroy_fn.take() {
            destroy_fn().expect("Failed to destroy FVM volume");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{RamdiskFactory, RAMDISK_FVM_SLICE_SIZE};
    use block_client::RemoteBlockClient;
    use fake_block_server::FakeServer;
    use fidl_fuchsia_fs_startup::VolumesMarker;
    use fs_management::Gpt;
    use ramdevice_client::{RamdiskClient, RamdiskClientBuilder};
    use std::sync::Arc;

    const BLOCK_SIZE: u64 = 4 * 1024;
    const BLOCK_COUNT: u64 = 1024;
    // We need more blocks for the GPT version of the test, since the library will by default
    // allocate 128MiB for the embedded FVM.  This is big enough for a 192MiB device.
    const GPT_BLOCK_COUNT: u64 = 49152;

    #[fuchsia::test]
    async fn ramdisk_create_block_device_with_zxcrypt() {
        let ramdisk_factory = RamdiskFactory::new(BLOCK_SIZE, BLOCK_COUNT).await;
        let _ = ramdisk_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: true,
                use_zxcrypt: true,
                volume_size: None,
            })
            .await;
    }

    #[fuchsia::test]
    async fn ramdisk_create_block_device_without_zxcrypt() {
        let ramdisk_factory = RamdiskFactory::new(BLOCK_SIZE, BLOCK_COUNT).await;
        let _ = ramdisk_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: true,
                use_zxcrypt: false,
                volume_size: None,
            })
            .await;
    }

    #[fuchsia::test]
    async fn ramdisk_create_block_device_without_volume_size() {
        let ramdisk_factory = RamdiskFactory::new(BLOCK_SIZE, BLOCK_COUNT).await;
        let ramdisk = ramdisk_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: true,
                use_zxcrypt: false,
                volume_size: None,
            })
            .await;
        let volume_info = ramdisk
            .connector()
            .connect_volume()
            .unwrap()
            .into_proxy()
            .get_volume_info()
            .await
            .unwrap();
        zx::ok(volume_info.0).unwrap();
        let volume_info = volume_info.2.unwrap();
        assert_eq!(volume_info.partition_slice_count, 1);
    }

    #[fuchsia::test]
    async fn ramdisk_create_block_device_with_volume_size() {
        let ramdisk_factory = RamdiskFactory::new(BLOCK_SIZE, BLOCK_COUNT).await;
        let ramdisk = ramdisk_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: false,
                use_zxcrypt: false,
                volume_size: Some(RAMDISK_FVM_SLICE_SIZE as u64 * 3),
            })
            .await;
        let volume_info = ramdisk
            .connector()
            .connect_volume()
            .unwrap()
            .into_proxy()
            .get_volume_info()
            .await
            .unwrap();
        zx::ok(volume_info.0).unwrap();
        let volume_info = volume_info.2.unwrap();
        assert_eq!(volume_info.partition_slice_count, 3);
    }

    async fn init_gpt(block_size: u32, block_count: u64) -> zx::Vmo {
        let vmo = zx::Vmo::create(block_size as u64 * block_count).unwrap();
        let server = Arc::new(FakeServer::from_vmo(
            block_size,
            vmo.create_child(zx::VmoChildOptions::REFERENCE, 0, 0).unwrap(),
        ));
        let (client, server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_hardware_block_volume::VolumeMarker>();

        let _task =
            fasync::Task::spawn(async move { server.serve(server_end.into_stream()).await });
        let client = Arc::new(RemoteBlockClient::new(client).await.unwrap());
        gpt::Gpt::format(client.clone(), vec![gpt::PartitionInfo::nil(); 128])
            .await
            .expect("format failed");
        vmo
    }

    struct FvmTestConfig {
        fxblob_enabled: bool,
        storage_host_enabled: bool,
    }

    // Retains test state.
    enum TestState {
        // The drivers are running in an isolated devmgr instance.
        Devfs(#[allow(dead_code)] RamdiskClient),
        // The ramdisk is running in an isolated devmgr instance, but the volume managers are
        // running in child components.
        StorageHost(
            #[allow(dead_code)] RamdiskClient,
            #[allow(dead_code)] ServingMultiVolumeFilesystem,
        ),
    }

    async fn initialize(config: FvmTestConfig) -> (TestState, BenchmarkVolumeFactory) {
        if config.fxblob_enabled {
            // Initialize a new GPT.
            let vmo = init_gpt(BLOCK_SIZE as u32, GPT_BLOCK_COUNT).await;
            let ramdisk_builder = RamdiskClientBuilder::new_with_vmo(vmo, Some(BLOCK_SIZE));
            let ramdisk = if config.storage_host_enabled {
                ramdisk_builder.use_v2()
            } else {
                ramdisk_builder
            }
            .build()
            .await
            .expect("Failed to create ramdisk");

            if config.storage_host_enabled {
                let gpt = fs_management::filesystem::Filesystem::from_boxed_config(
                    ramdisk.connector().unwrap(),
                    Box::new(Gpt::dynamic_child()),
                )
                .serve_multi_volume()
                .await
                .expect("Failed to serve GPT");
                let partitions =
                    Service::open_from_dir(gpt.exposed_dir(), fpartitions::PartitionServiceMarker)
                        .unwrap();
                let manager =
                    connect_to_protocol_at_dir_root::<fpartitions::PartitionsManagerMarker>(
                        gpt.exposed_dir(),
                    )
                    .unwrap();
                let fvm = BenchmarkVolumeFactory::connect_to_test_partition(partitions, manager)
                    .await
                    .expect("Failed to connect to FVM");
                (TestState::StorageHost(ramdisk, gpt), fvm)
            } else {
                ramdisk
                    .as_controller()
                    .expect("invalid controller")
                    .bind("gpt.cm")
                    .await
                    .expect("FIDL error calling bind()")
                    .map_err(zx::Status::from_raw)
                    .expect("bind() returned non-Ok status");
                wait_for_block_device_devfs(&[BlockDeviceMatcher::ContentsMatch(DiskFormat::Gpt)])
                    .await
                    .expect("Failed to wait for GPT to appear");
                let fvm = BenchmarkVolumeFactory::connect_to_test_partition_devfs()
                    .await
                    .expect("Failed to connect to FVM");
                (TestState::Devfs(ramdisk), fvm)
            }
        } else {
            // Initialize a new FVM.
            let ramdisk_builder = RamdiskClientBuilder::new(BLOCK_SIZE, BLOCK_COUNT);
            let ramdisk = if config.storage_host_enabled {
                ramdisk_builder.use_v2()
            } else {
                ramdisk_builder
            }
            .build()
            .await
            .expect("Failed to create ramdisk");
            fvm::format_for_fvm(&ramdisk.open().unwrap().into_proxy(), RAMDISK_FVM_SLICE_SIZE)
                .expect("Failed to format FVM");
            if config.storage_host_enabled {
                let fvm_component = match fs_management::filesystem::Filesystem::from_boxed_config(
                    ramdisk.connector().unwrap(),
                    Box::new(Fvm::dynamic_child()),
                )
                .serve_multi_volume()
                .await
                {
                    Ok(fvm_component) => fvm_component,
                    Err(_) => loop {},
                };
                let volumes_connector = {
                    let exposed_dir =
                        fuchsia_fs::directory::clone(fvm_component.exposed_dir()).unwrap();
                    Box::new(move || {
                        connect_to_protocol_at_dir_root::<VolumesMarker>(&exposed_dir).unwrap()
                    })
                };
                let volumes_dir_connector = {
                    let exposed_dir =
                        fuchsia_fs::directory::clone(fvm_component.exposed_dir()).unwrap();
                    Box::new(move || {
                        fuchsia_fs::directory::open_directory_async(
                            &exposed_dir,
                            "volumes",
                            fio::PERM_READABLE,
                        )
                        .unwrap()
                    })
                };
                let fvm = BenchmarkVolumeFactory::connect_to_system_fvm(
                    volumes_connector,
                    volumes_dir_connector,
                );
                (TestState::StorageHost(ramdisk, fvm_component), fvm.unwrap())
            } else {
                // Add a blob volume, since that is how we identify the system FVM partition.
                let block_controller = ramdisk.open_controller().unwrap().into_proxy();
                let fvm_path = block_controller
                    .get_topological_path()
                    .await
                    .expect("FIDL error")
                    .expect("Failed to get topo path");
                let dir = fuchsia_fs::directory::open_in_namespace(&fvm_path, fio::PERM_READABLE)
                    .unwrap();
                let volume_manager = fvm::start_fvm_driver(&block_controller, &dir)
                    .await
                    .expect("Failed to start FVM");
                let type_guid =
                    fidl_fuchsia_hardware_block_partition::Guid { value: BLOBFS_TYPE_GUID };
                let instance_guid =
                    fidl_fuchsia_hardware_block_partition::Guid { value: create_random_guid() };
                zx::ok(
                    volume_manager
                        .allocate_partition(1, &type_guid, &instance_guid, BLOBFS_VOLUME_NAME, 0)
                        .await
                        .expect("FIDL error"),
                )
                .expect("failed to allocate blobfs partition");
                wait_for_block_device_devfs(&[
                    BlockDeviceMatcher::Name(BLOBFS_VOLUME_NAME),
                    BlockDeviceMatcher::TypeGuid(&BLOBFS_TYPE_GUID),
                ])
                .await
                .expect("Failed to wait for blobfs to appear");
                let fvm = BenchmarkVolumeFactory::connect_to_system_fvm_devfs()
                    .await
                    .expect("Failed to connect to FVM");
                (TestState::Devfs(ramdisk), fvm)
            }
        }
    }

    async fn benchmark_volume_factory_can_find_fvm_instance(config: FvmTestConfig) {
        let (_state, volume_factory) = initialize(config).await;

        // Verify that a volume can be created.
        volume_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: true,
                use_zxcrypt: false,
                volume_size: None,
            })
            .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_can_find_fvm_instance_fvm_non_storage_host() {
        benchmark_volume_factory_can_find_fvm_instance(FvmTestConfig {
            fxblob_enabled: false,
            storage_host_enabled: false,
        })
        .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_can_find_fvm_instance_gpt_non_storage_host() {
        benchmark_volume_factory_can_find_fvm_instance(FvmTestConfig {
            fxblob_enabled: true,
            storage_host_enabled: false,
        })
        .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_can_find_fvm_instance_fvm() {
        benchmark_volume_factory_can_find_fvm_instance(FvmTestConfig {
            fxblob_enabled: false,
            storage_host_enabled: true,
        })
        .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_can_find_fvm_instance_gpt() {
        benchmark_volume_factory_can_find_fvm_instance(FvmTestConfig {
            fxblob_enabled: true,
            storage_host_enabled: true,
        })
        .await;
    }

    async fn dropping_an_fvm_volume_removes_the_volume(config: FvmTestConfig) {
        let (_state, volume_factory) = initialize(config).await;
        {
            let _volume = volume_factory
                .create_block_device(&BlockDeviceConfig {
                    requires_fvm: true,
                    use_zxcrypt: false,
                    volume_size: None,
                })
                .await;
            assert!(volume_factory.contains_fvm_volume(BENCHMARK_VOLUME_NAME).await);
        };
        assert!(!volume_factory.contains_fvm_volume(BENCHMARK_VOLUME_NAME).await);
    }

    #[fuchsia::test]
    async fn dropping_an_fvm_volume_removes_the_volume_fvm_non_storage_host() {
        dropping_an_fvm_volume_removes_the_volume(FvmTestConfig {
            fxblob_enabled: false,
            storage_host_enabled: false,
        })
        .await;
    }

    #[fuchsia::test]
    async fn dropping_an_fvm_volume_removes_the_volume_fvm() {
        dropping_an_fvm_volume_removes_the_volume(FvmTestConfig {
            fxblob_enabled: false,
            storage_host_enabled: true,
        })
        .await;
    }

    async fn benchmark_volume_factory_create_block_device_with_zxcrypt(config: FvmTestConfig) {
        let (_state, volume_factory) = initialize(config).await;
        let _ = volume_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: true,
                use_zxcrypt: true,
                volume_size: None,
            })
            .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_create_block_device_with_zxcrypt_fvm_non_storage_host() {
        benchmark_volume_factory_create_block_device_with_zxcrypt(FvmTestConfig {
            fxblob_enabled: false,
            storage_host_enabled: false,
        })
        .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_create_block_device_with_zxcrypt_gpt_non_storage_host() {
        benchmark_volume_factory_create_block_device_with_zxcrypt(FvmTestConfig {
            fxblob_enabled: true,
            storage_host_enabled: false,
        })
        .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_create_block_device_with_zxcrypt_fvm() {
        benchmark_volume_factory_create_block_device_with_zxcrypt(FvmTestConfig {
            fxblob_enabled: false,
            storage_host_enabled: true,
        })
        .await;
    }

    #[fuchsia::test]
    async fn benchmark_volume_factory_create_block_device_with_zxcrypt_gpt() {
        benchmark_volume_factory_create_block_device_with_zxcrypt(FvmTestConfig {
            fxblob_enabled: true,
            storage_host_enabled: true,
        })
        .await;
    }
}
