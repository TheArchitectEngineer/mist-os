// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::subsystems::prelude::*;
use anyhow::{bail, ensure, Context};
use assembly_config_capabilities::{Config, ConfigValueType};
use assembly_config_schema::platform_config::storage_config::StorageConfig;
use assembly_constants::{BootfsDestination, FileEntry};
use assembly_images_config::{
    BlobfsLayout, DataFilesystemFormat, FilesystemImageMode, FvmVolumeConfig, GptMode, VolumeConfig,
};

pub(crate) struct StorageSubsystemConfig;
impl DefineSubsystemConfiguration<StorageConfig> for StorageSubsystemConfig {
    fn define_configuration(
        context: &ConfigurationContext<'_>,
        storage_config: &StorageConfig,
        builder: &mut dyn ConfigurationBuilder,
    ) -> anyhow::Result<()> {
        if matches!(
            context.feature_set_level,
            FeatureSetLevel::Bootstrap | FeatureSetLevel::Embeddable
        ) {
            ensure!(
                storage_config.filesystems.image_mode == FilesystemImageMode::NoImage,
                "Bootstrap and Embeddable products must use filesystems.image_mode='no_image'"
            );
        }

        // Include legacy paver implementation in all feature sets above "embeddable" if the board
        // doesn't include it. Embeddable doesn't support paving.
        if *context.feature_set_level != FeatureSetLevel::Embeddable
            && !context.board_info.provides_feature("fuchsia::paver")
        {
            builder.platform_bundle("paver_legacy");
        }

        // Fetch a custom gen directory for placing temporary files. We get this
        // from the context, so that it can create unique gen directories for
        // each subsystem under the top-level assembly gen directory.
        let gendir = context.get_gendir().context("Getting gendir for storage subsystem")?;

        // Set the storage security policy/configuration for zxcrypt
        let zxcrypt_config_path = gendir.join("zxcrypt");

        if context.board_info.provides_feature("fuchsia::keysafe_ta") {
            std::fs::write(&zxcrypt_config_path, "tee")
        } else {
            std::fs::write(&zxcrypt_config_path, "null")
        }
        .context("Could not write zxcrypt configuration")?;

        builder
            .bootfs()
            .file(FileEntry {
                source: zxcrypt_config_path,
                destination: BootfsDestination::Zxcrypt,
            })
            .context("Adding zxcrypt config to bootfs")?;

        if *context.feature_set_level == FeatureSetLevel::Embeddable {
            // We don't need fshost in embeddable.
            return Ok(());
        }

        if storage_config.factory_data.enabled {
            builder.platform_bundle("factory_data");
        }

        if storage_config.mutable_storage_garbage_collection {
            context.ensure_feature_set_level(
                &[FeatureSetLevel::Standard],
                "Mutable storage garbage collection",
            )?;
            builder.platform_bundle("storage_cache_manager");
        }

        // Collect the arguments from the board.
        let blobfs_max_bytes = context.board_info.filesystems.fvm.blobfs.maximum_bytes.unwrap_or(0);
        let blobfs_initial_inodes =
            context.board_info.filesystems.fvm.blobfs.minimum_inodes.unwrap_or(0);
        let data_max_bytes = context.board_info.filesystems.fvm.minfs.maximum_bytes.unwrap_or(0);
        let fvm_slice_size = context.board_info.filesystems.fvm.slice_size.0;
        let gpt = context.board_info.filesystems.gpt.enabled();
        let gpt_all = context.board_info.filesystems.gpt_all
            || context.board_info.filesystems.gpt == GptMode::AllowMultiple;

        // Collect the arguments from the product.
        let ramdisk_image = storage_config.filesystems.image_mode == FilesystemImageMode::Ramdisk;
        let no_zxcrypt = storage_config.filesystems.no_zxcrypt;
        let format_data_on_corruption = storage_config.filesystems.format_data_on_corruption.0;
        let storage_host = storage_config.storage_host_enabled;
        let nand = storage_config.filesystems.watch_for_nand;

        // Prepare some default arguments that may get overridden by the product config.
        let mut blob_deprecated_padded = false;
        let mut use_disk_migration = false;
        let mut data_filesystem_format_str = "fxfs";
        let mut fxfs_blob = false;

        // Add all the AIBs and collect some argument values.
        builder.platform_bundle("fshost_common");
        builder.platform_bundle("fshost_storage");
        if storage_config.storage_host_enabled {
            match &storage_config.filesystems.volume {
                VolumeConfig::Fxfs => {
                    ensure!(gpt, "GPT required for Fxfs-based product assemblies");
                    fxfs_blob = true;
                    builder.platform_bundle("fshost_storage_host_fxfs");
                }
                VolumeConfig::Fvm(FvmVolumeConfig { blob, data, .. }) => {
                    blob_deprecated_padded = blob.blob_layout == BlobfsLayout::DeprecatedPadded;
                    match data.data_filesystem_format {
                        DataFilesystemFormat::Fxfs => {
                            bail!("Fxfs-in-FVM isn't supported in storage-host");
                        }
                        DataFilesystemFormat::F2fs => {
                            // TODO(https://fxbug.dev/339491886): Implement.
                            bail!("f2fs isn't supported yet in storage-host");
                        }
                        DataFilesystemFormat::Minfs => {
                            data_filesystem_format_str = "minfs";
                            // TODO(https://fxbug.dev/339491886): Implement support for migration
                            // images in storage-host.
                            ensure!(
                                !data.use_disk_based_minfs_migration,
                                "Migration isn't supported in storage-host yet",
                            );
                            if gpt {
                                builder.platform_bundle("fshost_storage_host_gpt_fvm_minfs");
                            } else {
                                builder.platform_bundle("fshost_storage_host_fvm_minfs");
                            }
                        }
                    }
                }
            }
        } else {
            // TODO(https://fxbug.dev/339491886): Delete the non-storage-host branch.
            match &storage_config.filesystems.volume {
                VolumeConfig::Fxfs => {
                    ensure!(gpt, "GPT required for Fxfs-based product assemblies");
                    fxfs_blob = true;
                    builder.platform_bundle("fshost_fxfs");
                }
                VolumeConfig::Fvm(FvmVolumeConfig { blob, data, .. }) => {
                    builder.platform_bundle("fshost_fvm");
                    blob_deprecated_padded = blob.blob_layout == BlobfsLayout::DeprecatedPadded;
                    match data.data_filesystem_format {
                        DataFilesystemFormat::Fxfs => {
                            builder.platform_bundle("fshost_fvm_fxfs");
                        }
                        DataFilesystemFormat::F2fs => {
                            data_filesystem_format_str = "f2fs";
                            builder.platform_bundle("fshost_fvm_f2fs");
                        }
                        DataFilesystemFormat::Minfs => {
                            data_filesystem_format_str = "minfs";
                            if data.use_disk_based_minfs_migration {
                                use_disk_migration = true;
                                builder.platform_bundle("fshost_fvm_minfs_migration");
                            } else {
                                builder.platform_bundle("fshost_fvm_minfs");
                            }
                        }
                    }
                }
            }
        }

        if context.build_type == &BuildType::Eng {
            builder.platform_bundle("fshost_eng");
        } else {
            builder.platform_bundle("fshost_non_eng");
        }

        let disable_automount =
            Config::new(ConfigValueType::Bool, storage_config.disable_automount.into());

        let starnix_volume_name = match &storage_config.starnix_volume.name {
            Some(name) => {
                if !fxfs_blob {
                    return Err(anyhow::anyhow!(
                        "Cannot have a starnix volume set for a non-fxblob configuration"
                    ));
                }
                Config::new(ConfigValueType::String { max_size: 64 }, name.clone().into())
            }
            None => Config::new(ConfigValueType::String { max_size: 64 }, "".into()),
        };
        let algorithm = match &storage_config.filesystems.blobfs_write_compression_algorithm {
            Some(algorithm) => Config::new(
                ConfigValueType::String { max_size: 20 },
                serde_json::to_value(algorithm)?,
            ),
            None => Config::new_void(),
        };
        let policy = match &storage_config.filesystems.blobfs_cache_eviction_policy {
            Some(policy) => {
                Config::new(ConfigValueType::String { max_size: 20 }, serde_json::to_value(policy)?)
            }
            None => Config::new_void(),
        };

        let configs = [
            ("fuchsia.fshost.Blobfs", Config::new_bool(true)),
            ("fuchsia.fshost.BlobfsMaxBytes", Config::new_uint64(blobfs_max_bytes)),
            ("fuchsia.fshost.BootPart", Config::new_bool(true)),
            ("fuchsia.fshost.CheckFilesystems", Config::new_bool(true)),
            ("fuchsia.fshost.Data", Config::new_bool(true)),
            ("fuchsia.fshost.DataMaxBytes", Config::new_uint64(data_max_bytes)),
            ("fuchsia.fshost.DisableBlockWatcher", Config::new_bool(false)),
            ("fuchsia.fshost.Factory", Config::new_bool(false)),
            ("fuchsia.fshost.Fvm", Config::new_bool(true)),
            ("fuchsia.fshost.RamdiskImage", Config::new_bool(ramdisk_image)),
            ("fuchsia.fshost.Gpt", Config::new_bool(gpt)),
            ("fuchsia.fshost.GptAll", Config::new_bool(gpt_all)),
            ("fuchsia.fshost.Mbr", Config::new_bool(false)),
            ("fuchsia.fshost.Netboot", Config::new_bool(false)),
            ("fuchsia.fshost.NoZxcrypt", Config::new_bool(no_zxcrypt)),
            ("fuchsia.fshost.FormatDataOnCorruption", Config::new_bool(format_data_on_corruption)),
            ("fuchsia.fshost.BlobfsInitialInodes", Config::new_uint64(blobfs_initial_inodes)),
            (
                "fuchsia.fshost.BlobfsUseDeprecatedPaddedFormat",
                Config::new_bool(blob_deprecated_padded),
            ),
            ("fuchsia.fshost.UseDiskMigration", Config::new_bool(use_disk_migration)),
            ("fuchsia.fshost.Nand", Config::new_bool(nand)),
            ("fuchsia.fshost.FxfsBlob", Config::new_bool(fxfs_blob)),
            ("fuchsia.fshost.StorageHost", Config::new_bool(storage_host)),
            ("fuchsia.fshost.FvmSliceSize", Config::new_uint64(fvm_slice_size)),
            (
                "fuchsia.fshost.DataFilesystemFormat",
                Config::new(
                    ConfigValueType::String { max_size: 64 },
                    data_filesystem_format_str.into(),
                ),
            ),
            (
                "fuchsia.fshost.FxfsCryptUrl",
                Config::new(
                    ConfigValueType::String { max_size: 64 },
                    "fuchsia-boot:///fxfs-crypt#meta/fxfs-crypt.cm".into(),
                ),
            ),
            ("fuchsia.fshost.DisableAutomount", disable_automount),
            ("fuchsia.fshost.StarnixVolumeName", starnix_volume_name),
            ("fuchsia.blobfs.WriteCompressionAlgorithm", algorithm),
            ("fuchsia.blobfs.CacheEvictionPolicy", policy),
        ];
        for config in configs {
            builder.set_config_capability(config.0, config.1)?;
        }

        // Include SDHCI driver through a platform AIB.
        if context.board_info.provides_feature("fuchsia::sdhci") {
            builder.platform_bundle("sdhci_driver");
        }

        // Include UFS driver through a platform AIB.
        if context.board_info.provides_feature("fuchsia::ufs") {
            builder.platform_bundle("ufs_driver");
        }

        Ok(())
    }
}
