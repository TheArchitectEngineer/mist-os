// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use delivery_blob::{delivery_blob_path, CompressionMode, Type1Blob};
use fidl::endpoints::{create_proxy, DiscoverableProtocolMarker as _};
use fidl_fuchsia_fs_startup::VolumeMarker as FsStartupVolumeMarker;
use fidl_fuchsia_fshost::AdminMarker;
use fidl_fuchsia_hardware_block_volume::{VolumeManagerMarker, VolumeMarker};
use fidl_fuchsia_update_verify::HealthStatus;
use fs_management::format::constants::DATA_PARTITION_LABEL;
use fs_management::partition::{find_partition_in, PartitionMatcher};
use fs_management::DATA_TYPE_GUID;
use fshost_test_fixture::disk_builder::{
    DataSpec, VolumesSpec, FVM_SLICE_SIZE, TEST_DISK_BLOCK_SIZE,
};
use fshost_test_fixture::{
    round_down, TestFixture, BLOBFS_MAX_BYTES, DATA_MAX_BYTES, VFS_TYPE_FXFS, VFS_TYPE_MEMFS,
    VFS_TYPE_MINFS,
};
use fuchsia_component::client::connect_to_named_protocol_at_dir_root;
use futures::FutureExt as _;
use regex::Regex;
use {fidl_fuchsia_fshost as fshost, fidl_fuchsia_io as fio, fuchsia_async as fasync};

#[cfg(feature = "fxblob")]
use {
    blob_writer::BlobWriter,
    fidl::endpoints::Proxy,
    fidl_fuchsia_fshost::StarnixVolumeProviderMarker,
    fidl_fuchsia_fxfs::{BlobCreatorMarker, BlobReaderMarker},
    fshost_test_fixture::STARNIX_VOLUME_NAME,
};

#[cfg(feature = "storage-host")]
use {
    fidl::endpoints::ServiceMarker as _, fidl_fuchsia_hardware_block_partition as fpartition,
    fidl_fuchsia_storage_partitions as fpartitions,
};

pub mod config;

use config::{
    blob_fs_type, data_fs_spec, data_fs_type, data_fs_zxcrypt, new_builder, volumes_spec,
    DATA_FILESYSTEM_VARIANT,
};

#[fuchsia::test]
async fn blobfs_and_data_mounted() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    // Also make sure tmpfs is getting exported.
    fixture.check_fs_type("tmp", VFS_TYPE_MEMFS).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;
    fixture.tear_down().await;
}

#[fuchsia::test]
async fn blobfs_and_data_mounted_legacy_label() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(volumes_spec())
        .format_data(data_fs_spec())
        .with_legacy_data_label();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_partition_nonexistent() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(VolumesSpec { create_data_partition: false, ..volumes_spec() });
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_legacy_label() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_legacy_data_label();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_no_fuchsia_boot() {
    let mut builder = new_builder().no_fuchsia_boot();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_with_small_initial_volume() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).data_volume_size(FVM_SLICE_SIZE);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_formatted_with_small_initial_volume_big_target() {
    let mut builder = new_builder();
    // The formatting uses the max bytes argument as the initial target to resize to. If this
    // target is larger than the disk, the resize should still succeed.
    builder.fshost().set_config_value(
        "data_max_bytes",
        fshost_test_fixture::disk_builder::DEFAULT_DISK_SIZE * 2,
    );
    builder.with_disk().format_volumes(volumes_spec()).data_volume_size(FVM_SLICE_SIZE);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

// Ensure WipeStorage is not supported in the normal mode of operation (i.e. when the
// `ramdisk_image` option is false). WipeStorage should only function within a recovery context.
#[fuchsia::test]
async fn wipe_storage_not_supported() {
    let builder = new_builder();
    let fixture = builder.build().await;

    let admin =
        fixture.realm.root.connect_to_protocol_at_exposed_dir::<fshost::AdminMarker>().unwrap();

    let (_, blobfs_server) = create_proxy::<fio::DirectoryMarker>();

    let result = admin
        .wipe_storage(Some(blobfs_server), None)
        .await
        .unwrap()
        .expect_err("WipeStorage unexpectedly succeeded");
    assert_eq!(zx::Status::from_raw(result), zx::Status::NOT_SUPPORTED);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn ramdisk_blob_and_data_mounted() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("ramdisk_image", true);
    builder
        .with_zbi_ramdisk()
        .format_volumes(volumes_spec())
        .format_data(DataSpec { zxcrypt: false, ..data_fs_spec() });
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn ramdisk_blob_and_data_mounted_no_existing_data_partition() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("ramdisk_image", true);
    builder
        .with_zbi_ramdisk()
        .format_volumes(VolumesSpec { create_data_partition: false, ..volumes_spec() })
        .format_data(DataSpec { zxcrypt: false, ..data_fs_spec() });
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn ramdisk_data_ignores_non_ramdisk() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("ramdisk_image", true);
    builder
        .with_disk()
        .format_volumes(volumes_spec())
        .format_data(DataSpec { zxcrypt: false, ..data_fs_spec() });
    let fixture = builder.build().await;

    // Make sure fvm is bound/launched when we expect
    // TODO(https://fxbug.dev/397770032): Once we support recovery mode for storage-host, it should
    // also wait for fvm to appear.
    if cfg!(not(any(feature = "storage-host", feature = "fxblob"))) {
        let ramdisk_dir = fixture.ramdisks[0].as_dir().expect("invalid dir proxy");
        device_watcher::recursive_wait(ramdisk_dir, "fvm/data-p-2/block").await.unwrap();
    }

    // There isn't really a good way to tell that something is not mounted, but at this point we
    // would be pretty close to it, so a timeout of a couple seconds should safeguard against
    // potential issues.
    futures::select! {
        _ = fixture.check_fs_type("data", data_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - data was mounted");
        },
        _ = fixture.check_fs_type("blob", blob_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - blob was mounted");
        },
        _ = fasync::Timer::new(std::time::Duration::from_secs(2)).fuse() => (),
    }

    fixture.tear_down().await;
}

#[fuchsia::test]
// There is an equivalent test for storage-host below (they are almost entirely different so it's
// not worth having them in the same test)
#[cfg_attr(any(feature = "fxblob", feature = "storage-host"), ignore)]
async fn partition_max_size_set() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", DATA_MAX_BYTES)
        .set_config_value("blobfs_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    // Get the blobfs instance guid.
    // TODO(https://fxbug.dev/42072287): Remove hardcoded paths
    let volume_proxy_data = connect_to_named_protocol_at_dir_root::<VolumeMarker>(
        &fixture.dir("dev-topological", fio::Flags::empty()),
        "sys/platform/ram-disk/ramctl/ramdisk-0/block/fvm/blobfs-p-1/block",
    )
    .unwrap();
    let (status, data_instance_guid) = volume_proxy_data.get_instance_guid().await.unwrap();
    zx::Status::ok(status).unwrap();
    let mut blobfs_instance_guid = data_instance_guid.unwrap();

    let data_matcher = PartitionMatcher {
        type_guids: Some(vec![DATA_TYPE_GUID]),
        labels: Some(vec![DATA_PARTITION_LABEL.to_string()]),
        ignore_if_path_contains: Some("zxcrypt/unsealed".to_string()),
        ..Default::default()
    };

    let dev = fixture.dir("dev-topological/class/block", fio::Flags::empty());
    let data_partition_controller =
        find_partition_in(&dev, data_matcher, zx::MonotonicDuration::from_seconds(10))
            .await
            .expect("failed to find data partition");

    // Get the data instance guid.
    let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
    data_partition_controller.connect_to_device_fidl(volume_server_end.into_channel()).unwrap();

    let (status, data_instance_guid) = volume_proxy.get_instance_guid().await.unwrap();
    zx::Status::ok(status).unwrap();
    let mut data_instance_guid = data_instance_guid.unwrap();

    // TODO(https://fxbug.dev/42072287): Remove hardcoded paths
    let fvm_proxy = connect_to_named_protocol_at_dir_root::<VolumeManagerMarker>(
        &fixture.dir("dev-topological", fio::Flags::empty()),
        "sys/platform/ram-disk/ramctl/ramdisk-0/block/fvm",
    )
    .unwrap();

    // blobfs max size check
    let (status, blobfs_slice_count) =
        fvm_proxy.get_partition_limit(blobfs_instance_guid.as_mut()).await.unwrap();
    zx::Status::ok(status).unwrap();
    assert_eq!(blobfs_slice_count, (BLOBFS_MAX_BYTES + FVM_SLICE_SIZE - 1) / FVM_SLICE_SIZE);

    // data max size check
    let (status, data_slice_count) =
        fvm_proxy.get_partition_limit(data_instance_guid.as_mut()).await.unwrap();
    zx::Status::ok(status).unwrap();
    // The expected size depends on whether we are using zxcrypt or not.
    // When wrapping in zxcrypt the data partition size is the same, but the physical disk
    // commitment is one slice bigger.
    let mut expected_slices = (DATA_MAX_BYTES + FVM_SLICE_SIZE - 1) / FVM_SLICE_SIZE;
    if data_fs_zxcrypt() && data_fs_type() != VFS_TYPE_FXFS {
        log::info!("Adding an extra expected data slice for zxcrypt");
        expected_slices += 1;
    }
    assert_eq!(data_slice_count, expected_slices);

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(not(any(feature = "fxblob", feature = "storage-host")), ignore)]
// For fxblob and all storage-host configurations, volume limits are set via the
// fuchsia.fs.startup.Volume protocol.
async fn set_volume_limit() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", DATA_MAX_BYTES)
        .set_config_value("blobfs_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volumes_dir = fixture.dir("volumes", fio::Flags::empty());
    let blob_volume_name = if cfg!(feature = "fxblob") { "blob" } else { "blobfs" };
    let blob_volume_proxy = connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(
        &volumes_dir,
        blob_volume_name,
    )
    .unwrap();
    let blobfs_limit =
        blob_volume_proxy.get_limit().await.unwrap().map_err(zx::Status::from_raw).unwrap();
    let expected_blobfs_limit = if cfg!(feature = "fxblob") {
        BLOBFS_MAX_BYTES
    } else {
        // The fvm component rounds the max bytes down to the nearest slice size.
        (BLOBFS_MAX_BYTES / FVM_SLICE_SIZE) * FVM_SLICE_SIZE
    };
    assert_eq!(blobfs_limit, expected_blobfs_limit);
    let data_volume_proxy =
        connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(&volumes_dir, "data")
            .unwrap();
    let data_limit =
        data_volume_proxy.get_limit().await.unwrap().map_err(zx::Status::from_raw).unwrap();
    let expected_data_limit = if cfg!(feature = "fxblob") {
        DATA_MAX_BYTES
    } else {
        // The fvm component rounds the max bytes down to the nearest slice size.
        (DATA_MAX_BYTES / FVM_SLICE_SIZE) * FVM_SLICE_SIZE
    };
    assert_eq!(data_limit, expected_data_limit);

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn create_unmount_and_remount_starnix_volume() {
    let mut builder = new_builder();
    builder
        .fshost()
        .create_starnix_volume_crypt()
        .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .create(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("create failed");

    let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
        &exposed_dir_proxy,
        "root",
        fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to open the root dir of the starnix volume");

    let starnix_volume_file = fuchsia_fs::directory::open_file(
        &starnix_volume_root_dir,
        "file",
        fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to create file in starnix volume");
    fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();
    volume_provider.unmount().await.expect("fidl transport error").expect("unmount failed");

    let disk = fixture.tear_down().await.unwrap();
    let mut builder = new_builder().with_disk_from(disk);
    builder
        .fshost()
        .create_starnix_volume_crypt()
        .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .mount(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("mount failed");

    let starnix_volume_root_dir =
        fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
            .await
            .expect("Failed to open the root dir of the starnix volume");

    let starnix_volume_file =
        fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
            .await
            .expect("Failed to create file in starnix volume");
    assert_eq!(&fuchsia_fs::file::read(&starnix_volume_file).await.unwrap()[..], b"file contents!");

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn create_starnix_volume_wipes_previous_volume() {
    let mut builder = new_builder();
    builder
        .fshost()
        .create_starnix_volume_crypt()
        .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .create(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("create failed");

    let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
        &exposed_dir_proxy,
        "root",
        fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to open the root dir of the starnix volume");

    let starnix_volume_file = fuchsia_fs::directory::open_file(
        &starnix_volume_root_dir,
        "file",
        fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to create file in starnix volume");
    fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();
    volume_provider.unmount().await.expect("fidl transport error").expect("unmount failed");

    let disk = fixture.tear_down().await.unwrap();
    let mut builder = new_builder().with_disk_from(disk);
    builder
        .fshost()
        .create_starnix_volume_crypt()
        .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .create(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("create failed");

    let starnix_volume_root_dir =
        fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
            .await
            .expect("Failed to open the root dir of the starnix volume");

    assert_matches!(
        fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
            .await
            .expect_err("StarnixVolumeProvider.Create should wipe the Starnix volume if it exists"),
        fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
    );

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(not(feature = "fxblob"), ignore)]
async fn set_volume_bytes_limit() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", DATA_MAX_BYTES)
        .set_config_value("blobfs_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volumes_dir = fixture.dir("volumes", fio::Flags::empty());

    let blob_volume_proxy =
        connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(&volumes_dir, "blob")
            .unwrap();
    let blob_volume_bytes_limit = blob_volume_proxy.get_limit().await.unwrap().unwrap();

    let data_volume_proxy =
        connect_to_named_protocol_at_dir_root::<FsStartupVolumeMarker>(&volumes_dir, "data")
            .unwrap();
    let data_volume_bytes_limit = data_volume_proxy.get_limit().await.unwrap().unwrap();
    assert_eq!(blob_volume_bytes_limit, BLOBFS_MAX_BYTES);
    assert_eq!(data_volume_bytes_limit, DATA_MAX_BYTES);
    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(feature = "fxblob", ignore)]
async fn set_data_and_blob_max_bytes_zero() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("data_max_bytes", 0).set_config_value("blobfs_max_bytes", 0);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    let flags = fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE;

    let data_root = fixture.dir("data", flags);
    let file = fuchsia_fs::directory::open_file(&data_root, "file", flags).await.unwrap();
    fuchsia_fs::file::write(&file, "file contents!").await.unwrap();

    let blob_contents = vec![0; 8192];
    let hash = fuchsia_merkle::from_slice(&blob_contents).root();

    let blob_root = fixture.dir("blob", flags);
    let blob =
        fuchsia_fs::directory::open_file(&blob_root, &format!("{}", hash), flags).await.unwrap();
    blob.resize(blob_contents.len() as u64)
        .await
        .expect("FIDL call failed")
        .expect("truncate failed");
    fuchsia_fs::file::write(&blob, &blob_contents).await.unwrap();

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn set_data_and_blob_max_bytes_zero_new_write_api() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("data_max_bytes", 0).set_config_value("blobfs_max_bytes", 0);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    let flags = fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE;

    let data_root = fixture.dir("data", flags);
    let file = fuchsia_fs::directory::open_file(&data_root, "file", flags).await.unwrap();
    fuchsia_fs::file::write(&file, "file contents!").await.unwrap();

    let blob_contents = vec![0; 8192];
    let hash = fuchsia_merkle::from_slice(&blob_contents).root();
    let compressed_data: Vec<u8> = Type1Blob::generate(&blob_contents, CompressionMode::Always);

    let blob_proxy = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<BlobCreatorMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed");

    let writer_client_end = blob_proxy
        .create(&hash.into(), false)
        .await
        .expect("transport error on BlobCreator.Create")
        .expect("failed to create blob");
    let writer = writer_client_end.into_proxy();
    let mut blob_writer = BlobWriter::create(writer, compressed_data.len() as u64)
        .await
        .expect("failed to create BlobWriter");
    blob_writer.write(&compressed_data).await.unwrap();

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn netboot_set() {
    // Set the netboot flag
    let mut builder = new_builder().netboot();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    // Make sure fvm is bound/launched when we expect
    // TODO(https://fxbug.dev/397770032): Once we support recovery mode for storage-host, it should
    // also wait for fvm to appear.
    if cfg!(not(any(feature = "storage-host", feature = "fxblob"))) {
        let ramdisk_dir = fixture.ramdisks[0].as_dir().expect("invalid dir proxy");
        device_watcher::recursive_wait(ramdisk_dir, "fvm/data-p-2/block").await.unwrap();
    }

    // Use the same approach as ramdisk_data_ignores_non_ramdisk() to ensure that
    // neither blobfs nor data were mounted using a timeout
    futures::select! {
        _ = fixture.check_fs_type("data", data_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - data was mounted");
        },
        _ = fixture.check_fs_type("blob", blob_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - blob was mounted");
        },
        _ = fasync::Timer::new(std::time::Duration::from_secs(2)).fuse() => {
        },
    }

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn ramdisk_image_serves_zbi_ramdisk_contents_with_unformatted_data() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_zbi_ramdisk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(feature = "fxfs", ignore)]
async fn shred_data_volume_not_supported() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let admin = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<AdminMarker>()
        .expect("connect_to_protcol_at_exposed_dir failed");

    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect_err("shred_data_volume should fail");

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(not(feature = "fxfs"), ignore)]
async fn shred_data_volume_when_mounted() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fuchsia_fs::directory::open_file(
        &fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE),
        "test-file",
        fio::Flags::FLAG_MAYBE_CREATE,
    )
    .await
    .expect("open_file failed");

    let admin = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<AdminMarker>()
        .expect("connect_to_protcol_at_exposed_dir failed");

    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect("shred_data_volume failed");

    let disk = fixture.tear_down().await.unwrap();

    let fixture = new_builder().with_disk_from(disk).build().await;

    // If we try and open the same test file, it shouldn't exist because the data volume should have
    // been shredded.
    assert_matches!(
        fuchsia_fs::directory::open_file(
            &fixture.dir("data", fio::PERM_READABLE),
            "test-file",
            fio::PERM_READABLE,
        )
        .await
        .expect_err("open_file failed"),
        fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
    );

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn shred_data_deletes_starnix_volume() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    builder
        .fshost()
        .create_starnix_volume_crypt()
        .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    // Need to connect to the StarnixVolumeProvider protocol that fshost exposes and Mount the
    // starnix volume.
    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (_exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .create(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("create failed");

    let admin = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<AdminMarker>()
        .expect("connect_to_protcol_at_exposed_dir failed");

    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect("shred_data_volume failed");
    let disk = fixture.tear_down().await.unwrap();

    let mut builder = new_builder().with_disk_from(disk);
    builder
        .fshost()
        .create_starnix_volume_crypt()
        .set_config_value("starnix_volume_name", STARNIX_VOLUME_NAME);
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let volumes_dir = fixture.dir("volumes", fio::Flags::empty());
    let dir_entries =
        fuchsia_fs::directory::readdir(&volumes_dir).await.expect("Failed to readdir the volumes");
    assert!(dir_entries.iter().find(|x| x.name.contains(STARNIX_VOLUME_NAME)).is_none());

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn vend_a_fresh_starnix_test_volume_on_each_mount() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec());
    builder.fshost().create_starnix_volume_crypt();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    // Need to connect to the StarnixVolumeProvider protocol that fshost exposes and Mount the
    // starnix volume.
    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .mount(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("mount failed");

    let starnix_volume_root_dir = fuchsia_fs::directory::open_directory(
        &exposed_dir_proxy,
        "root",
        fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to open the root dir of the starnix volume");

    let starnix_volume_file = fuchsia_fs::directory::open_file(
        &starnix_volume_root_dir,
        "file",
        fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to create file in starnix volume");
    fuchsia_fs::file::write(&starnix_volume_file, "file contents!").await.unwrap();
    volume_provider.unmount().await.expect("fidl transport error").expect("unmount failed");

    let disk = fixture.tear_down().await.unwrap();
    let mut builder = new_builder().with_disk_from(disk);
    builder.fshost().create_starnix_volume_crypt();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    // Need to connect to the StarnixVolumeProvider protocol that fshost exposes and Mount the
    // starnix volume.
    let volume_provider = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<StarnixVolumeProviderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed for the StarnixVolumeProvider protocol");
    let (crypt, _crypt_management) = fixture.setup_starnix_crypt().await;
    let (exposed_dir_proxy, exposed_dir_server) =
        fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    volume_provider
        .mount(crypt.into_client_end().unwrap(), exposed_dir_server)
        .await
        .expect("fidl transport error")
        .expect("mount failed");

    let starnix_volume_root_dir =
        fuchsia_fs::directory::open_directory(&exposed_dir_proxy, "root", fio::PERM_READABLE)
            .await
            .expect("Failed to open the root dir of the starnix volume");

    // fshost should vend a fresh Starnix test volume on every mount so this file should no longer
    // exist.
    fuchsia_fs::directory::open_file(&starnix_volume_root_dir, "file", fio::PERM_READABLE)
        .await
        .expect_err("fshost should vend a fresh Starnix test volume on every mount");

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(not(feature = "fxfs"), ignore)]
async fn shred_data_volume_from_recovery() {
    let mut builder = new_builder();
    builder.with_disk().with_gpt().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    fuchsia_fs::directory::open_file(
        &fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE),
        "test-file",
        fio::Flags::FLAG_MAYBE_CREATE,
    )
    .await
    .expect("open_file failed");

    let disk = fixture.tear_down().await.unwrap();

    // Launch a version of fshost that will behave like recovery: it will mount data and blob from
    // a ramdisk it launches, binding the fvm on the "regular" disk but otherwise leaving it alone.
    let mut builder = new_builder().with_disk_from(disk);
    builder.fshost().set_config_value("ramdisk_image", true);
    builder.with_zbi_ramdisk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let admin = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<AdminMarker>()
        .expect("connect_to_protcol_at_exposed_dir failed");

    admin
        .shred_data_volume()
        .await
        .expect("shred_data_volume FIDL failed")
        .expect("shred_data_volume failed");

    let disk = fixture.tear_down().await.unwrap();

    let fixture = new_builder().with_disk_from(disk).build().await;

    // If we try and open the same test file, it shouldn't exist because the data volume should have
    // been shredded.
    assert_matches!(
        fuchsia_fs::directory::open_file(
            &fixture.dir("data", fio::PERM_READABLE),
            "test-file",
            fio::PERM_READABLE
        )
        .await
        .expect_err("open_file failed"),
        fuchsia_fs::node::OpenError::OpenError(zx::Status::NOT_FOUND)
    );

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn disable_block_watcher() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("disable_block_watcher", true);
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    // The filesystems are not mounted when the block watcher is disabled.
    futures::select! {
        _ = fixture.check_fs_type("data", data_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - data was mounted");
        },
        _ = fixture.check_fs_type("blob", blob_fs_type()).fuse() => {
            panic!("check_fs_type returned unexpectedly - blob was mounted");
        },
        _ = fasync::Timer::new(std::time::Duration::from_secs(2)).fuse() => (),
    }

    fixture.tear_down().await;
}

async fn assert_volumes_are_expected(fixture: &TestFixture) {
    let (volumes_dir, expected) = if cfg!(feature = "fxblob") {
        // This includes fxblob in storage-host and non-storage-host configurations
        (fixture.dir("volumes", fio::Flags::empty()), vec![r"^blob$", r"^data$", r"^unencrypted$"])
    } else if cfg!(feature = "storage-host") {
        (fixture.dir("volumes", fio::Flags::empty()), vec![r"^blobfs$", r"^data$"])
    } else {
        (
            fuchsia_fs::directory::open_directory(
                &fixture.dir("dev-topological", fio::Flags::empty()),
                "sys/platform/ram-disk/ramctl/ramdisk-0/block/fvm",
                fio::Flags::empty(),
            )
            .await
            .expect("Failed to open the fvm"),
            vec![
                r"^blobfs",
                r"data",
                r"^device_controller$",
                r"^device_protocol$",
                r"^device_topology$",
            ],
        )
    };

    let mut expected: Vec<_> = expected.into_iter().map(|r| Regex::new(r).unwrap()).collect();

    // Ensure that the account and virtualization volumes were successfully destroyed. The volumes
    // are removed from devfs asynchronously, so use a timeout.
    let start_time = std::time::Instant::now();
    let mut dir_entries =
        fuchsia_fs::directory::readdir(&volumes_dir).await.expect("Failed to readdir the volumes");
    while dir_entries
        .iter()
        .find(|x| x.name.contains("account") || x.name.contains("virtualization"))
        .is_some()
    {
        let elapsed = start_time.elapsed().as_secs() as u64;
        if elapsed >= 30 {
            panic!("The account or virtualization partition still exists in devfs after 30 secs");
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        dir_entries = fuchsia_fs::directory::readdir(&volumes_dir)
            .await
            .expect("Failed to readdir the fvm DirectoryProxy");
    }
    for entry in dir_entries {
        let name = entry.name;
        let position = expected
            .iter()
            .position(|r| r.is_match(&name))
            .unwrap_or_else(|| panic!("Unexpected entry name: {name}"));
        expected.swap_remove(position);
    }
    assert!(expected.is_empty(), "Missing {expected:?}");
}

#[fuchsia::test]
async fn reset_volumes() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(volumes_spec())
        .with_extra_volume("account")
        .with_extra_volume("virtualization");
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    assert_volumes_are_expected(&fixture).await;

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn reset_volumes_no_existing_data_volume() {
    let mut builder = new_builder();
    builder
        .with_disk()
        .format_volumes(VolumesSpec { create_data_partition: false, ..volumes_spec() })
        .with_extra_volume("account")
        .with_extra_volume("virtualization");
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    assert_volumes_are_expected(&fixture).await;

    fixture.tear_down().await;
}

// Toggle migration mode

#[fuchsia::test]
// TODO(https://fxbug.dev/397763081): support disk migration on storage-host
#[cfg_attr(any(feature = "fxblob", feature = "storage-host"), ignore)]
async fn migration_toggle() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", DATA_MAX_BYTES)
        .set_config_value("use_disk_migration", true);
    builder
        .with_disk()
        .data_volume_size(round_down(DATA_MAX_BYTES / 2, FVM_SLICE_SIZE))
        .format_volumes(volumes_spec())
        .format_data(DataSpec { format: Some("minfs"), zxcrypt: true, ..Default::default() })
        .set_fs_switch("toggle");
    let fixture = builder.build().await;

    fixture.check_fs_type("data", VFS_TYPE_FXFS).await;
    fixture.check_test_data_file().await;

    let disk = fixture.tear_down().await.unwrap();

    let mut builder = new_builder().with_disk_from(disk);
    builder.fshost().set_config_value("data_max_bytes", DATA_MAX_BYTES / 2);
    let fixture = builder.build().await;

    fixture.check_fs_type("data", VFS_TYPE_MINFS).await;
    fixture.check_test_data_file().await;
    fixture.tear_down().await;
}

#[fuchsia::test]
// TODO(https://fxbug.dev/397763081): support disk migration on storage-host
#[cfg_attr(any(feature = "fxblob", feature = "storage-host"), ignore)]
async fn migration_to_fxfs() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", DATA_MAX_BYTES / 2)
        .set_config_value("use_disk_migration", true);
    builder
        .with_disk()
        .data_volume_size(round_down(DATA_MAX_BYTES / 2, FVM_SLICE_SIZE))
        .format_volumes(volumes_spec())
        .format_data(DataSpec { format: Some("minfs"), zxcrypt: true, ..Default::default() })
        .set_fs_switch("fxfs");
    let fixture = builder.build().await;

    fixture.check_fs_type("data", VFS_TYPE_FXFS).await;
    fixture.check_test_data_file().await;
    fixture.tear_down().await;
}

#[fuchsia::test]
// TODO(https://fxbug.dev/397763081): support disk migration on storage-host
#[cfg_attr(any(feature = "fxblob", feature = "storage-host"), ignore)]
async fn migration_to_minfs() {
    let mut builder = new_builder();
    builder
        .fshost()
        .set_config_value("data_max_bytes", DATA_MAX_BYTES / 2)
        .set_config_value("use_disk_migration", true);
    builder
        .with_disk()
        .data_volume_size(round_down(DATA_MAX_BYTES / 2, FVM_SLICE_SIZE))
        .format_volumes(volumes_spec())
        .format_data(DataSpec { format: Some("fxfs"), zxcrypt: false, ..Default::default() })
        .set_fs_switch("minfs");
    let fixture = builder.build().await;

    fixture.check_fs_type("data", VFS_TYPE_MINFS).await;
    fixture.check_test_data_file().await;
    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn health_check_blobs() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    let blobfs_health_check = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<fidl_fuchsia_update_verify::ComponentOtaHealthCheckMarker>()
        .expect("connect_to_protcol_at_exposed_dir failed");
    let status = blobfs_health_check.get_health_status().await.expect("FIDL failure");
    assert_eq!(status, HealthStatus::Healthy);

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg_attr(feature = "fxblob", ignore)]
async fn delivery_blob_support() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("blobfs_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;
    // 65536 bytes of 0xff "small" f75f59a944d2433bc6830ec243bfefa457704d2aed12f30539cd4f18bf1d62cf
    const HASH: &'static str = "f75f59a944d2433bc6830ec243bfefa457704d2aed12f30539cd4f18bf1d62cf";
    let data: Vec<u8> = vec![0xff; 65536];
    let payload = Type1Blob::generate(&data, CompressionMode::Always);
    // `data` is highly compressible, so we should be able to transfer it in one write call.
    assert!((payload.len() as u64) < fio::MAX_TRANSFER_SIZE, "Payload exceeds max transfer size!");
    // Now attempt to write `payload` as we would any other blob, but using the delivery path.
    let blob = fuchsia_fs::directory::open_file(
        &fixture.dir("blob", fio::PERM_READABLE | fio::PERM_WRITABLE),
        &delivery_blob_path(HASH),
        fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE,
    )
    .await
    .expect("Failed to open delivery blob for writing.");
    // Resize to required length.
    blob.resize(payload.len() as u64).await.unwrap().map_err(zx::Status::from_raw).unwrap();
    // Write payload.
    let bytes_written = blob.write(&payload).await.unwrap().map_err(zx::Status::from_raw).unwrap();
    assert_eq!(bytes_written, payload.len() as u64);
    blob.close().await.unwrap().map_err(zx::Status::from_raw).unwrap();

    // We should now be able to open the blob by its hash and read the contents back.
    let blob = fuchsia_fs::directory::open_file(
        &fixture.dir("blob", fio::PERM_READABLE),
        HASH,
        fio::PERM_READABLE,
    )
    .await
    .expect("Failed to open delivery blob for reading.");
    // Read the last 1024 bytes of the file and ensure the bytes match the original `data`.
    let len: u64 = 1024;
    let offset: u64 = data.len().checked_sub(1024).unwrap() as u64;
    let contents = blob.read_at(len, offset).await.unwrap().map_err(zx::Status::from_raw).unwrap();
    assert_eq!(contents.as_slice(), &data[offset as usize..]);

    fixture.tear_down().await;
}

#[fuchsia::test]
#[cfg(feature = "fxblob")]
async fn delivery_blob_support_fxblob() {
    let mut builder = new_builder();
    builder.fshost().set_config_value("blobfs_max_bytes", BLOBFS_MAX_BYTES);
    builder.with_disk().format_volumes(volumes_spec());
    let fixture = builder.build().await;

    let data: Vec<u8> = vec![0xff; 65536];
    let hash = fuchsia_merkle::from_slice(&data).root();
    let payload = Type1Blob::generate(&data, CompressionMode::Always);

    let blob_creator = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<BlobCreatorMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed");
    let blob_writer_client_end = blob_creator
        .create(&hash.into(), false)
        .await
        .expect("transport error on create")
        .expect("failed to create blob");

    let writer = blob_writer_client_end.into_proxy();
    let mut blob_writer = BlobWriter::create(writer, payload.len() as u64)
        .await
        .expect("failed to create BlobWriter");
    blob_writer.write(&payload).await.unwrap();

    // We should now be able to open the blob by its hash and read the contents back.
    let blob_reader = fixture
        .realm
        .root
        .connect_to_protocol_at_exposed_dir::<BlobReaderMarker>()
        .expect("connect_to_protocol_at_exposed_dir failed");
    let vmo = blob_reader.get_vmo(&hash.into()).await.unwrap().unwrap();

    // Read the last 1024 bytes of the file and ensure the bytes match the original `data`.
    let mut buf = vec![0; 1024];
    let offset: u64 = data.len().checked_sub(1024).unwrap() as u64;
    let () = vmo.read(&mut buf, offset).unwrap();
    assert_eq!(&buf, &data[offset as usize..]);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn data_persists() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let data_root = fixture.dir("data", fio::PERM_READABLE | fio::PERM_WRITABLE);
    let file = fuchsia_fs::directory::open_file(
        &data_root,
        "file",
        fio::Flags::FLAG_MUST_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
    )
    .await
    .unwrap();
    fuchsia_fs::file::write(&file, "file contents!").await.unwrap();

    // Shut down fshost, which should propagate to the data filesystem too.
    let disk = fixture.tear_down().await.unwrap();
    let builder = new_builder().with_disk_from(disk);
    let fixture = builder.build().await;

    fixture.check_fs_type("data", data_fs_type()).await;

    let data_root = fixture.dir("data", fio::PERM_READABLE);
    let file =
        fuchsia_fs::directory::open_file(&data_root, "file", fio::PERM_READABLE).await.unwrap();
    assert_eq!(&fuchsia_fs::file::read(&file).await.unwrap()[..], b"file contents!");

    fixture.tear_down().await;
}

#[cfg(feature = "storage-host")]
async fn gpt_num_partitions(fixture: &TestFixture) -> usize {
    let partitions = fixture.dir(
        fidl_fuchsia_storage_partitions::PartitionServiceMarker::SERVICE_NAME,
        fuchsia_fs::PERM_READABLE,
    );
    fuchsia_fs::directory::readdir(&partitions).await.expect("Failed to read partitions").len()
}

#[cfg(not(feature = "storage-host"))]
async fn gpt_num_partitions(fixture: &TestFixture) -> usize {
    let gpt_dir = fuchsia_fs::directory::open_directory(
        &fixture.dir("dev-topological", fio::Flags::empty()),
        "sys/platform/ram-disk/ramctl/ramdisk-0/block",
        fio::Flags::empty(),
    )
    .await
    .expect("Failed to open the gpt device");
    fuchsia_fs::directory::readdir(&gpt_dir)
        .await
        .expect("Failed to read partitions")
        .into_iter()
        .filter(|entry| entry.name.starts_with("part-"))
        .count()
}

#[fuchsia::test]
async fn initialized_gpt() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    // TODO(https://fxbug.dev/399197713): re-enable extra disk once flake is fixed
    // builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;
    fixture.check_test_data_file().await;
    fixture.check_test_blob(DATA_FILESYSTEM_VARIANT == "fxblob").await;

    assert_eq!(gpt_num_partitions(&fixture).await, 1);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn uninitialized_gpt() {
    let mut builder = new_builder().with_uninitialized_disk();
    builder.fshost().set_config_value("netboot", true);
    // TODO(https://fxbug.dev/399197713): re-enable extra disk once flake is fixed
    // builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 0);

    fixture.tear_down().await;
}

#[cfg(feature = "storage-host")]
#[fuchsia::test]
async fn reset_uninitialized_gpt() {
    let mut builder = new_builder().with_uninitialized_disk();
    builder.fshost().set_config_value("netboot", true);
    // TODO(https://fxbug.dev/399197713): re-enable extra disk once flake is fixed
    // builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 0);

    let recovery =
        fixture.realm.root.connect_to_protocol_at_exposed_dir::<fshost::RecoveryMarker>().unwrap();
    recovery
        .init_system_partition_table(&[fpartitions::PartitionInfo {
            name: "part".to_string(),
            type_guid: fpartition::Guid { value: [0xabu8; 16] },
            instance_guid: fpartition::Guid { value: [0xcdu8; 16] },
            start_block: 4,
            num_blocks: 1,
            flags: 0,
        }])
        .await
        .expect("FIDL error")
        .expect("init_system_partition_table failed");

    assert_eq!(gpt_num_partitions(&fixture).await, 1);

    fixture.tear_down().await;
}

#[cfg(feature = "storage-host")]
#[fuchsia::test]
async fn reset_initialized_gpt() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    builder.fshost().set_config_value("netboot", true);
    // TODO(https://fxbug.dev/399197713): re-enable extra disk once flake is fixed
    // builder.with_extra_disk().set_uninitialized();
    let fixture = builder.build().await;

    assert_eq!(gpt_num_partitions(&fixture).await, 1);

    let recovery =
        fixture.realm.root.connect_to_protocol_at_exposed_dir::<fshost::RecoveryMarker>().unwrap();
    recovery
        .init_system_partition_table(&[
            fpartitions::PartitionInfo {
                name: "part".to_string(),
                type_guid: fpartition::Guid { value: [0xabu8; 16] },
                instance_guid: fpartition::Guid { value: [0xcdu8; 16] },
                start_block: 4,
                num_blocks: 1,
                flags: 0,
            },
            fpartitions::PartitionInfo {
                name: "part2".to_string(),
                type_guid: fpartition::Guid { value: [0x11u8; 16] },
                instance_guid: fpartition::Guid { value: [0x22u8; 16] },
                start_block: 5,
                num_blocks: 1,
                flags: 0,
            },
        ])
        .await
        .expect("FIDL error")
        .expect("init_system_partition_table failed");

    assert_eq!(gpt_num_partitions(&fixture).await, 2);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn health_check_service() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    let proxy = fuchsia_component::client::connect_to_protocol_at_dir_root::<
        fidl_fuchsia_update_verify::ComponentOtaHealthCheckMarker,
    >(fixture.exposed_dir())
    .unwrap();
    let status = proxy.get_health_status().await.expect("FIDL error");
    assert_eq!(status, HealthStatus::Healthy);

    fixture.tear_down().await;
}

#[fuchsia::test]
async fn debug_block_directory() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).format_data(data_fs_spec());
    let fixture = builder.build().await;

    // Make sure the filesystems are enumerated before trying to access the block devices. The
    // debug directory is populated as the devices are emitted by the watcher.
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let block = fuchsia_fs::directory::open_directory(
        fixture.exposed_dir(),
        "debug_block",
        fio::PERM_READABLE,
    )
    .await
    .unwrap();

    // Check that the block directory contains some of the required things for the shell tools
    let source =
        fuchsia_fs::directory::open_file(&block, "000/source", fio::PERM_READABLE).await.unwrap();
    // This is a smoke check - we can't check for a concrete source because it's different (and
    // potentially unstable) depending on the configuration, and it's not that useful to be a
    // change detector.
    assert!(fuchsia_fs::file::read_to_string(&source).await.unwrap().len() > 0);

    let volume = connect_to_named_protocol_at_dir_root::<VolumeMarker>(
        &block,
        "000/fuchsia.hardware.block.volume.Volume",
    )
    .unwrap();
    assert_eq!(
        volume.get_info().await.unwrap().map_err(zx::Status::from_raw).unwrap().block_size,
        512,
    );

    fixture.tear_down().await;
}

// TODO(https://fxbug.dev/399197713): Enable this test when extra disks don't flake
#[ignore]
#[fuchsia::test]
async fn expose_unmanaged_block_devices() {
    let mut builder = new_builder();
    builder.with_disk().format_volumes(volumes_spec()).with_gpt().format_data(data_fs_spec());
    builder.with_extra_disk().set_uninitialized().size(8192);
    let fixture = builder.build().await;

    // Make sure the filesystems are enumerated before trying to access the block devices. The block
    // directory is populated as the devices are emitted by the watcher.
    fixture.check_fs_type("blob", blob_fs_type()).await;
    fixture.check_fs_type("data", data_fs_type()).await;

    let block_dir =
        fuchsia_fs::directory::open_directory(fixture.exposed_dir(), "block", fio::PERM_READABLE)
            .await
            .unwrap();
    let mut dirents = fuchsia_fs::directory::readdir(&block_dir).await.expect("readdir failed");
    let device_path = dirents.pop().unwrap().name;
    assert!(dirents.is_empty(), "Multiple devices published");

    let path = format!("{}/{}", &device_path, VolumeMarker::PROTOCOL_NAME);
    let volume = fuchsia_component::client::connect_to_named_protocol_at_dir_root::<VolumeMarker>(
        &block_dir, &path,
    )
    .unwrap();
    let metadata =
        volume.get_metadata().await.expect("FIDL error").expect("Failed to get metadata");
    assert_eq!(metadata.num_blocks, Some(8192 / TEST_DISK_BLOCK_SIZE as u64));

    fixture.tear_down().await;
}
