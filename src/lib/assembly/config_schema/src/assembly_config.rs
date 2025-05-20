// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::image_assembly_config::PartialKernelConfig;
use crate::platform_config::PlatformConfig;
use crate::PackageDetails;
use anyhow::Result;
use assembly_constants::{CompiledPackageDestination, FileEntry};
use assembly_container::{assembly_container, AssemblyContainer, WalkPaths};
use assembly_file_relative_path::{FileRelativePathBuf, SupportsFileRelativePaths};
use assembly_package_utils::PackageInternalPathBuf;
use camino::Utf8PathBuf;
use fuchsia_pkg::PackageManifest;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::common::{DriverDetails, PackageName};
use crate::product_config::{ProductConfig, ProductPackageDetails};

/// Configuration for a Product Assembly operation.
///
/// This is a high-level operation that takes a more abstract description of
/// what is desired in the assembled product images, and then generates the
/// complete Image Assembly configuration (`crate::config::ImageAssemblyConfig`)
/// from that.
#[derive(Debug, Deserialize, Serialize, JsonSchema, WalkPaths)]
#[serde(deny_unknown_fields)]
#[assembly_container(product_configuration.json)]
pub struct AssemblyConfig {
    #[walk_paths]
    pub platform: PlatformConfig,
    #[walk_paths]
    #[serde(default)]
    pub product: ProductConfig,
}

impl AssemblyConfig {
    pub fn add_package_names(mut self) -> Result<Self> {
        self.product.packages.base = Self::add_package_names_to_set(self.product.packages.base)?;
        self.product.packages.cache = Self::add_package_names_to_set(self.product.packages.cache)?;
        Ok(self)
    }

    fn add_package_names_to_set(
        set: BTreeMap<String, ProductPackageDetails>,
    ) -> Result<BTreeMap<String, ProductPackageDetails>> {
        set.into_values()
            .map(|pkg| {
                let manifest = PackageManifest::try_load_from(&pkg.manifest)?;
                let name = manifest.name().to_string();
                Ok((name, pkg))
            })
            .collect()
    }
}

/// A typename to represent a package that contains shell command binaries,
/// and the paths to those binaries
pub type ShellCommands = BTreeMap<PackageName, BTreeSet<PackageInternalPathBuf>>;

/// A bundle of inputs to be used in the assembly of a product.
#[derive(
    Debug, Default, Deserialize, Serialize, PartialEq, SupportsFileRelativePaths, WalkPaths,
)]
#[serde(default, deny_unknown_fields)]
pub struct AssemblyInputBundle {
    /// The parameters that specify which kernel to put into the ZBI.
    pub kernel: Option<PartialKernelConfig>,

    /// The qemu kernel to use when starting the emulator.
    pub qemu_kernel: Option<Utf8PathBuf>,

    /// The list of additional boot args to add.
    pub boot_args: Vec<String>,

    /// The packages that are in the bootfs package list, which are
    /// added to the BOOTFS in the ZBI.
    pub bootfs_packages: Vec<Utf8PathBuf>,

    /// The set of files to be placed in BOOTFS in the ZBI.
    pub bootfs_files: Vec<FileEntry<String>>,

    /// Package entries that internally specify their package set, instead of being grouped
    /// separately.
    pub packages: Vec<PackageDetails>,

    /// Entries for the `config_data` package.
    pub config_data: BTreeMap<String, Vec<FileEntry<String>>>,

    /// The blobs index of the AIB.  This currently isn't used by product
    /// assembly, as the package manifests contain the same information.
    pub blobs: Vec<Utf8PathBuf>,

    /// Configuration of base driver packages. Driver packages should not be
    /// listed in the base package list and will be included automatically.
    pub base_drivers: Vec<DriverDetails>,

    /// Configuration of boot driver packages. Driver packages should not be
    /// listed in the bootfs package list and will be included automatically.
    pub boot_drivers: Vec<DriverDetails>,

    /// Map of the names of packages that contain shell commands to the list of
    /// commands within each.
    pub bootfs_shell_commands: ShellCommands,

    /// Map of the names of packages that contain shell commands to the list of
    /// commands within each.
    pub shell_commands: ShellCommands,

    /// Packages to create dynamically as part of the Assembly process.
    #[file_relative_paths]
    #[walk_paths]
    pub packages_to_compile: Vec<CompiledPackageDefinition>,

    /// A package that includes files to include in bootfs.
    pub bootfs_files_package: Option<Utf8PathBuf>,

    /// A list of memory buckets to pass to memory monitor.
    #[file_relative_paths]
    #[walk_paths]
    pub memory_buckets: Vec<FileRelativePathBuf>,
}

impl AssemblyInputBundle {
    /// Are all containers in this AIB empty.
    pub fn is_empty(&self) -> bool {
        // destructure to ensure that when new fields are added, they require this function
        // to be touched.
        let Self {
            kernel,
            qemu_kernel,
            boot_args,
            bootfs_packages,
            bootfs_files,
            packages,
            config_data,
            blobs,
            base_drivers,
            boot_drivers,
            bootfs_shell_commands,
            shell_commands,
            packages_to_compile,
            bootfs_files_package,
            memory_buckets,
        } = &self;

        qemu_kernel.is_none()
            && boot_args.is_empty()
            && bootfs_packages.is_empty()
            && bootfs_files.is_empty()
            && packages.is_empty()
            && config_data.is_empty()
            && blobs.is_empty()
            && base_drivers.is_empty()
            && boot_drivers.is_empty()
            && bootfs_shell_commands.is_empty()
            && shell_commands.is_empty()
            && packages_to_compile.is_empty()
            && bootfs_files_package.is_none()
            && memory_buckets.is_empty()
            && (match &kernel {
                Some(kernel) => kernel.args.is_empty() && kernel.path.is_none(),
                None => true,
            })
    }
}

/// Contents of a compiled package. The contents provided by all
/// selected AIBs are merged by `name` into a single package
/// at assembly time.
#[derive(Debug, Deserialize, Serialize, PartialEq, SupportsFileRelativePaths, WalkPaths)]
#[serde(deny_unknown_fields)]
pub struct CompiledPackageDefinition {
    /// Name of the package to compile.
    pub name: CompiledPackageDestination,

    /// Components to compile and add to the package.
    #[file_relative_paths]
    #[walk_paths]
    #[serde(default)]
    pub components: Vec<CompiledComponentDefinition>,

    /// Non-component files to add to the package.
    #[serde(default)]
    pub contents: Vec<FileEntry<String>>,

    /// CML files included by the component cml.
    #[serde(default)]
    pub includes: Vec<Utf8PathBuf>,

    /// Whether the contents of this package should go into bootfs.
    /// Gated by allowlist -- please use this as a base package if possible.
    #[serde(default)]
    pub bootfs_package: bool,
}

/// Contents of a compiled component. The contents provided by all
/// selected AIBs are merged by `name` into a single package
/// at assembly time.
#[derive(Debug, Deserialize, Serialize, PartialEq, SupportsFileRelativePaths, WalkPaths)]
#[serde(deny_unknown_fields)]
pub struct CompiledComponentDefinition {
    /// The name of the component to compile.
    pub component_name: String,

    /// CML file shards to include in the compiled component manifest.
    #[file_relative_paths]
    #[walk_paths]
    pub shards: Vec<FileRelativePathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::PackageSet;
    use crate::platform_config::media_config::{
        AudioConfig, AudioDeviceRegistryConfig, PlatformMediaConfig,
    };
    use crate::platform_config::{BuildType, FeatureSetLevel};
    use crate::product_config::ProductPackageDetails;
    use assembly_util as util;
    use fuchsia_pkg::{MetaPackage, PackageManifestBuilder, PackageName};
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn test_product_assembly_config_from_json5() {
        let json5 = r#"
        {
          platform: {
            build_type: "eng",
          },
          product: {},
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::Eng);
        assert_eq!(platform.feature_set_level, FeatureSetLevel::Standard);
    }

    #[test]
    fn test_bringup_product_assembly_config_from_json5() {
        let json5 = r#"
        {
          platform: {
            feature_set_level: "bootstrap",
            build_type: "eng",
          },
          product: {},
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::Eng);
        assert_eq!(platform.feature_set_level, FeatureSetLevel::Bootstrap);
    }

    #[test]
    fn test_minimal_product_assembly_config_from_json5() {
        let json5 = r#"
        {
          platform: {
            build_type: "eng",
          },
          product: {},
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::Eng);
        assert_eq!(platform.feature_set_level, FeatureSetLevel::Standard);
    }

    #[test]
    fn test_empty_product_assembly_config_from_json5() {
        let json5 = r#"
        {
          platform: {
            feature_set_level: "test_no_platform",
            build_type: "eng",
          },
          product: {},
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::Eng);
        assert_eq!(platform.feature_set_level, FeatureSetLevel::TestNoPlatform);
    }

    #[test]
    fn test_buildtype_deserialization_userdebug() {
        let json5 = r#"
        {
          platform: {
            build_type: "userdebug",
          },
          product: {},
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::UserDebug);
    }

    #[test]
    fn test_buildtype_deserialization_user() {
        let json5 = r#"
        {
          platform: {
            build_type: "user",
          },
          product: {},
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::User);
    }

    #[test]
    fn test_product_assembly_config_with_product_provided_parts() {
        let json5 = r#"
        {
          platform: {
            build_type: "eng",
          },
          product: {
              packages: {
                  base: [
                      { manifest: "path/to/base/package_manifest.json" }
                  ],
                  cache: [
                      { manifest: "path/to/cache/package_manifest.json" }
                  ],
              },
              base_drivers: [
                {
                  package: "path/to/base/driver/package_manifest.json",
                  components: [ "meta/path/to/component.cml" ]
                }
              ]
          },
        }
    "#;

        let mut cursor = std::io::Cursor::new(json5);
        let config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        let platform = config.platform;
        assert_eq!(platform.build_type, BuildType::Eng);
        assert_eq!(
            config.product.packages.base,
            [(
                "0".to_string(),
                ProductPackageDetails {
                    manifest: "path/to/base/package_manifest.json".into(),
                    config_data: Vec::default()
                }
            )]
            .into()
        );
        assert_eq!(
            config.product.packages.cache,
            [(
                "0".to_string(),
                ProductPackageDetails {
                    manifest: "path/to/cache/package_manifest.json".into(),
                    config_data: Vec::default()
                }
            )]
            .into()
        );
        assert_eq!(
            config.product.base_drivers,
            vec![DriverDetails {
                package: FileRelativePathBuf::FileRelative(
                    "path/to/base/driver/package_manifest.json".into()
                ),
                components: vec!["meta/path/to/component.cml".into()]
            }]
        )
    }

    #[test]
    fn test_product_assembly_config_with_relative_paths() {
        let dir = tempdir().unwrap();
        let dir_path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let config_path = dir_path.join("product_configuration.json");
        let config_file = std::fs::File::create(&config_path).unwrap();

        let index_path = dir_path.join("component_id_index.json");
        std::fs::write(&index_path, "").unwrap();

        let json = serde_json::json!({
          "platform": {
            "build_type": "eng",
            "storage": {
              "component_id_index": {
                "product_index": "component_id_index.json",
              },
            },
          },
        });
        serde_json::to_writer(config_file, &json).unwrap();
        let config = AssemblyConfig::from_dir(&dir_path).unwrap();

        assert_eq!(index_path, config.platform.storage.component_id_index.product_index.unwrap());
    }

    #[test]
    fn test_assembly_input_bundle_from_json5() {
        let json5 = r#"
            {
              // json5 files can have comments in them.
              packages: [
                {
                    package: "package5",
                    set: "base",
                },
                {
                    package: "package6",
                    set: "cache",
                },
              ],
              kernel: {
                path: "path/to/kernel",
                args: ["arg1", "arg2"],
              },
              // and lists can have trailing commas
              boot_args: ["arg1", "arg2", ],
              bootfs_files: [
                {
                  source: "path/to/source",
                  destination: "path/to/destination",
                }
              ],
              config_data: {
                "package1": [
                  {
                    source: "path/to/source.json",
                    destination: "config.json"
                  }
                ]
              },
              base_drivers: [
                {
                  package: "path/to/driver",
                  components: ["path/to/1234", "path/to/5678"]
                }
              ],
              shell_commands: {
                "package1": ["path/to/binary1", "path/to/binary2"]
              },
              packages_to_compile: [
                  {
                    name: "core",
                    components: [
                      {
                        component_name: "component1",
                        shards: ["path/to/component1.cml"],
                      },
                      {
                        component_name: "component2",
                        shards: ["path/to/component2.cml"],
                      },
                    ],
                    contents: [
                        {
                            source: "path/to/source",
                            destination: "path/to/destination",
                        }
                    ],
                    includes: [ "src/path/to/include.cml" ]
                },
              ],
              memory_buckets: [
                "path/to/buckets.json",
              ],
            }
        "#;
        let bundle =
            util::from_reader::<_, AssemblyInputBundle>(&mut std::io::Cursor::new(json5)).unwrap();
        assert_eq!(
            bundle.packages,
            vec!(
                PackageDetails {
                    package: FileRelativePathBuf::FileRelative(Utf8PathBuf::from("package5")),
                    set: PackageSet::Base,
                },
                PackageDetails {
                    package: FileRelativePathBuf::FileRelative(Utf8PathBuf::from("package6")),
                    set: PackageSet::Cache,
                },
            )
        );
        let expected_kernel = PartialKernelConfig {
            path: Some(Utf8PathBuf::from("path/to/kernel")),
            args: vec!["arg1".to_string(), "arg2".to_string()],
        };
        assert_eq!(bundle.kernel, Some(expected_kernel));
        assert_eq!(bundle.boot_args, vec!("arg1".to_string(), "arg2".to_string()));
        assert_eq!(
            bundle.bootfs_files,
            vec!(FileEntry {
                source: Utf8PathBuf::from("path/to/source"),
                destination: "path/to/destination".to_string()
            })
        );
        assert_eq!(
            bundle.config_data.get("package1").unwrap(),
            &vec!(FileEntry {
                source: Utf8PathBuf::from("path/to/source.json"),
                destination: "config.json".to_string()
            })
        );
        assert_eq!(
            bundle.base_drivers[0],
            DriverDetails {
                package: FileRelativePathBuf::FileRelative(Utf8PathBuf::from("path/to/driver")),
                components: vec!(
                    Utf8PathBuf::from("path/to/1234"),
                    Utf8PathBuf::from("path/to/5678")
                )
            }
        );
        assert_eq!(
            bundle.shell_commands.get("package1").unwrap(),
            &BTreeSet::from([
                PackageInternalPathBuf::from("path/to/binary1"),
                PackageInternalPathBuf::from("path/to/binary2"),
            ])
        );
        assert_eq!(
            bundle.memory_buckets,
            vec![FileRelativePathBuf::FileRelative("path/to/buckets.json".into())]
        );
    }

    #[test]
    fn test_assembly_config_wrapper_for_overrides() {
        let config: AssemblyConfig = serde_json::from_value(serde_json::json!({
            "platform": {
                "build_type": "eng",
            },
            "product": {},
        }))
        .unwrap();

        let overrides = serde_json::json!({
            "platform": {
                "media": {
                    "audio": {
                      "device_registry": {},
                    },
                },
            },
        });

        let config = config.apply_overrides(overrides).unwrap();

        assert_eq!(
            config.platform.media,
            PlatformMediaConfig {
                audio: Some(AudioConfig::DeviceRegistry(AudioDeviceRegistryConfig {
                    eager_start: false
                })),
                ..Default::default()
            },
        );
    }

    #[test]
    fn test_get_package_names() {
        // Prepare a directory for temporary files.
        let dir = tempdir().unwrap();
        let dir_path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        // Generate a fake package manifest.
        let package_name = PackageName::from_str("my_pkg").unwrap();
        let meta_package = MetaPackage::from_name_and_variant_zero(package_name);
        let package_manifest_builder = PackageManifestBuilder::new(meta_package);
        let package_manifest = package_manifest_builder.build();
        let package_manifest_path = dir_path.join("my_pkg_package_manifest.json");
        package_manifest.write_with_relative_paths(&package_manifest_path).unwrap();

        // Create an assembly config with the package manifest.
        let json5 = r#"
        {
          platform: {
            build_type: "eng",
          },
        }
        "#;
        let mut cursor = std::io::Cursor::new(json5);
        let mut config: AssemblyConfig = util::from_reader(&mut cursor).unwrap();
        config.product.packages.base.insert(
            "0".to_string(),
            ProductPackageDetails { manifest: package_manifest_path.clone(), config_data: vec![] },
        );

        // Test the logic to add proper package names.
        let config = config.add_package_names().unwrap();
        let details = config.product.packages.base.get("my_pkg").unwrap();
        assert_eq!(&details.manifest, &package_manifest_path);
    }
}
