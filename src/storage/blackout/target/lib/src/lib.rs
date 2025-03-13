// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! library for target side of filesystem integrity host-target interaction tests

#![deny(missing_docs)]

use anyhow::{anyhow, Context as _, Result};
use async_trait::async_trait;
use fidl::endpoints::create_proxy;
use fidl::HandleBased as _;
use fidl_fuchsia_blackout_test::{ControllerRequest, ControllerRequestStream};
use fidl_fuchsia_device::ControllerMarker;
use fidl_fuchsia_hardware_block_volume::VolumeManagerMarker;
use fs_management::filesystem::BlockConnector;
use fs_management::format::DiskFormat;
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at_path, Service};
use fuchsia_component::server::{ServiceFs, ServiceObj};
use fuchsia_fs::directory::readdir;
use futures::{future, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use rand::rngs::StdRng;
use rand::{distributions, Rng, SeedableRng};
use std::pin::pin;
use std::sync::Arc;
use storage_isolated_driver_manager::{
    create_random_guid, find_block_device, find_block_device_devfs, into_guid,
    wait_for_block_device_devfs, BlockDeviceMatcher, Guid,
};
use {
    fidl_fuchsia_io as fio, fidl_fuchsia_storage_partitions as fpartitions, fuchsia_async as fasync,
};

pub mod static_tree;

/// The three steps the target-side of a blackout test needs to implement.
#[async_trait]
pub trait Test {
    /// Setup the test run on the given block_device.
    async fn setup(
        self: Arc<Self>,
        device_label: String,
        device_path: Option<String>,
        seed: u64,
    ) -> Result<()>;
    /// Run the test body on the given device_path.
    async fn test(
        self: Arc<Self>,
        device_label: String,
        device_path: Option<String>,
        seed: u64,
    ) -> Result<()>;
    /// Verify the consistency of the filesystem on the device_path.
    async fn verify(
        self: Arc<Self>,
        device_label: String,
        device_path: Option<String>,
        seed: u64,
    ) -> Result<()>;
}

struct BlackoutController(ControllerRequestStream);

/// A test server, which serves the fuchsia.blackout.test.Controller protocol.
pub struct TestServer<'a, T> {
    fs: ServiceFs<ServiceObj<'a, BlackoutController>>,
    test: Arc<T>,
}

impl<'a, T> TestServer<'a, T>
where
    T: Test + 'static,
{
    /// Create a new test server for this test.
    pub fn new(test: T) -> Result<TestServer<'a, T>> {
        let mut fs = ServiceFs::new();
        fs.dir("svc").add_fidl_service(BlackoutController);
        fs.take_and_serve_directory_handle()?;

        Ok(TestServer { fs, test: Arc::new(test) })
    }

    /// Start serving the outgoing directory. Blocks until all connections are closed.
    pub async fn serve(self) {
        const MAX_CONCURRENT: usize = 10_000;
        let test = self.test;
        self.fs
            .for_each_concurrent(MAX_CONCURRENT, move |stream| {
                handle_request(test.clone(), stream).unwrap_or_else(|e| log::error!("{}", e))
            })
            .await;
    }
}

async fn handle_request<T: Test + 'static>(
    test: Arc<T>,
    BlackoutController(mut stream): BlackoutController,
) -> Result<()> {
    while let Some(request) = stream.try_next().await? {
        handle_controller(test.clone(), request).await?;
    }

    Ok(())
}

async fn handle_controller<T: Test + 'static>(
    test: Arc<T>,
    request: ControllerRequest,
) -> Result<()> {
    match request {
        ControllerRequest::Setup { responder, device_label, device_path, seed } => {
            let res = test.setup(device_label, device_path, seed).await.map_err(|e| {
                log::error!("{:?}", e);
                zx::Status::INTERNAL.into_raw()
            });
            responder.send(res)?;
        }
        ControllerRequest::Test { responder, device_label, device_path, seed, duration } => {
            let test_fut = test.test(device_label, device_path, seed).map_err(|e| {
                log::error!("{:?}", e);
                zx::Status::INTERNAL.into_raw()
            });
            if duration != 0 {
                // If a non-zero duration is provided, spawn the test and then return after that
                // duration.
                log::info!("starting test and replying in {} seconds...", duration);
                let timer = pin!(fasync::Timer::new(std::time::Duration::from_secs(duration)));
                let res = match future::select(test_fut, timer).await {
                    future::Either::Left((res, _)) => res,
                    future::Either::Right((_, test_fut)) => {
                        fasync::Task::spawn(test_fut.map(|_| ())).detach();
                        Ok(())
                    }
                };
                responder.send(res)?;
            } else {
                // If a zero duration is provided, return once the test step is complete.
                log::info!("starting test...");
                responder.send(test_fut.await)?;
            }
        }
        ControllerRequest::Verify { responder, device_label, device_path, seed } => {
            let res = test.verify(device_label, device_path, seed).await.map_err(|e| {
                // The test tries failing on purpose, so only print errors as warnings.
                log::warn!("{:?}", e);
                zx::Status::BAD_STATE.into_raw()
            });
            responder.send(res)?;
        }
    }

    Ok(())
}

/// Generate a Vec<u8> of random bytes from a seed using a standard distribution.
pub fn generate_content(seed: u64) -> Vec<u8> {
    let mut rng = StdRng::seed_from_u64(seed);

    let size = rng.gen_range(1..1 << 16);
    rng.sample_iter(&distributions::Standard).take(size).collect()
}

/// Find the device in /dev/class/block that represents a given topological path. Returns the full
/// path of the device in /dev/class/block.
pub async fn find_dev(dev: &str) -> Result<String> {
    let dev_class_block =
        fuchsia_fs::directory::open_in_namespace("/dev/class/block", fio::PERM_READABLE)?;
    for entry in readdir(&dev_class_block).await? {
        let path = format!("/dev/class/block/{}", entry.name);
        let proxy = connect_to_protocol_at_path::<ControllerMarker>(&path)?;
        let topo_path = proxy.get_topological_path().await?.map_err(|s| zx::Status::from_raw(s))?;
        log::info!("{} => {}", path, topo_path);
        if dev == topo_path {
            return Ok(path);
        }
    }
    Err(anyhow::anyhow!("Couldn't find {} in /dev/class/block", dev))
}

/// Returns a directory proxy connected to /dev.
pub fn dev() -> fio::DirectoryProxy {
    fuchsia_fs::directory::open_in_namespace("/dev", fio::PERM_READABLE)
        .expect("failed to open /dev")
}

/// This type guid is only used if the test has to create the gpt partition itself. Otherwise, only
/// the label is used to find the partition.
const BLACKOUT_TYPE_GUID: &Guid = &[
    0x68, 0x45, 0x23, 0x01, 0xab, 0x89, 0xef, 0xcd, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];

const GPT_PARTITION_SIZE: u64 = 60 * 1024 * 1024;

/// Set up a partition for testing using the device label, returning a block connector for it. If
/// the partition already exists with this label, it's used. If no existing device is found with
/// this label, create a new gpt partition to use. If storage-host is enabled, it uses the new
/// partition apis from fshost, if not it falls back to devfs.
pub async fn set_up_partition(
    device_label: String,
    storage_host: bool,
) -> Result<Box<dyn BlockConnector>> {
    if !storage_host {
        return set_up_partition_devfs(device_label).await;
    }

    let partitions = Service::open(fpartitions::PartitionServiceMarker).unwrap();
    let manager = connect_to_protocol::<fpartitions::PartitionsManagerMarker>().unwrap();

    let service_instances =
        partitions.clone().enumerate().await.expect("Failed to enumerate partitions");
    if let Some(connector) =
        find_block_device(&[BlockDeviceMatcher::Name(&device_label)], service_instances.into_iter())
            .await
            .context("Failed to find block device")?
    {
        log::info!(device_label:%; "found existing partition");
        Ok(Box::new(connector))
    } else {
        log::info!(device_label:%; "adding new partition to the system gpt");
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
            name: Some(device_label.clone()),
            type_guid: Some(into_guid(BLACKOUT_TYPE_GUID.clone())),
            instance_guid: Some(into_guid(create_random_guid())),
            num_blocks: Some(GPT_PARTITION_SIZE / info.1 as u64),
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
            partitions.enumerate().await.expect("Failed to enumerate partitions");
        let connector = find_block_device(
            &[BlockDeviceMatcher::Name(&device_label)],
            service_instances.into_iter(),
        )
        .await
        .context("Failed to find block device")?
        .unwrap();
        Ok(Box::new(connector))
    }
}

/// Fallback logic for setting up a partition on devfs.
/// TODO(https://fxbug.dev/394968352): remove when everything uses storage-host.
async fn set_up_partition_devfs(device_label: String) -> Result<Box<dyn BlockConnector>> {
    let mut partition_path = if let Ok(path) =
        find_block_device_devfs(&[BlockDeviceMatcher::Name(&device_label)]).await
    {
        log::info!("found existing partition");
        path
    } else {
        log::info!("finding existing gpt and adding a new partition to it");
        let mut gpt_block_path =
            find_block_device_devfs(&[BlockDeviceMatcher::ContentsMatch(DiskFormat::Gpt)])
                .await
                .context("finding gpt device failed")?;
        gpt_block_path.push("device_controller");
        let gpt_block_controller =
            connect_to_protocol_at_path::<ControllerMarker>(gpt_block_path.to_str().unwrap())
                .context("connecting to block controller")?;
        let gpt_path = gpt_block_controller
            .get_topological_path()
            .await
            .context("get_topo fidl error")?
            .map_err(zx::Status::from_raw)
            .context("get_topo failed")?;
        let gpt_controller = connect_to_protocol_at_path::<ControllerMarker>(&format!(
            "{}/gpt/device_controller",
            gpt_path
        ))
        .context("connecting to gpt controller")?;

        let (volume_manager, server) = create_proxy::<VolumeManagerMarker>();
        gpt_controller
            .connect_to_device_fidl(server.into_channel())
            .context("connecting to gpt fidl")?;
        let slice_size = {
            let (status, info) = volume_manager.get_info().await.context("get_info fidl error")?;
            zx::ok(status).context("get_info returned error")?;
            info.unwrap().slice_size
        };
        let slice_count = GPT_PARTITION_SIZE / slice_size;
        let instance_guid = into_guid(create_random_guid());
        let status = volume_manager
            .allocate_partition(
                slice_count,
                &into_guid(BLACKOUT_TYPE_GUID.clone()),
                &instance_guid,
                &device_label,
                0,
            )
            .await
            .context("allocating test partition fidl error")?;
        zx::ok(status).context("allocating test partition returned error")?;

        wait_for_block_device_devfs(&[
            BlockDeviceMatcher::Name(&device_label),
            BlockDeviceMatcher::TypeGuid(&BLACKOUT_TYPE_GUID),
        ])
        .await
        .context("waiting for new gpt partition")?
    };
    partition_path.push("device_controller");
    log::info!(partition_path:?; "found partition to use");
    Ok(Box::new(
        connect_to_protocol_at_path::<ControllerMarker>(partition_path.to_str().unwrap())
            .context("connecting to provided path")?,
    ))
}

/// Find an existing test partition using the device label and return a block connector for it. If
/// storage-host is enabled, use the new partition service apis from fshost, otherwise fall back to
/// devfs.
pub async fn find_partition(
    device_label: String,
    storage_host: bool,
) -> Result<Box<dyn BlockConnector>> {
    if !storage_host {
        return find_partition_devfs(device_label).await;
    }

    let partitions = Service::open(fpartitions::PartitionServiceMarker).unwrap();
    let service_instances = partitions.enumerate().await.expect("Failed to enumerate partitions");
    let connector = find_block_device(
        &[BlockDeviceMatcher::Name(&device_label)],
        service_instances.into_iter(),
    )
    .await
    .context("Failed to find block device")?
    .ok_or_else(|| anyhow!("Block device not found"))?;
    log::info!(device_label:%; "found existing partition");
    Ok(Box::new(connector))
}

/// Fallback logic for finding a partition on devfs.
/// TODO(https://fxbug.dev/394968352): remove when everything uses storage-host.
async fn find_partition_devfs(device_label: String) -> Result<Box<dyn BlockConnector>> {
    log::info!("finding gpt");
    let mut partition_path = find_block_device_devfs(&[BlockDeviceMatcher::Name(&device_label)])
        .await
        .context("finding block device")?;
    partition_path.push("device_controller");
    log::info!(partition_path:?; "found partition to use");
    Ok(Box::new(
        connect_to_protocol_at_path::<ControllerMarker>(partition_path.to_str().unwrap())
            .context("connecting to provided path")?,
    ))
}
