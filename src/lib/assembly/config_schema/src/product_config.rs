// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;

use anyhow::Result;
use assembly_container::{FileType, WalkPaths, WalkPathsFn};
use assembly_package_utils::{PackageInternalPathBuf, PackageManifestPathBuf};
use camino::{Utf8Path, Utf8PathBuf};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{path_schema, vec_path_schema, DriverDetails};

/// The Product-provided configuration details.
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema, WalkPaths, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ProductConfig {
    #[walk_paths]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub packages: ProductPackagesConfig,

    /// List of base drivers to include in the product.
    #[walk_paths]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub base_drivers: Vec<DriverDetails>,

    /// Product-specific session information.
    ///
    /// Default to None which creates a "paused" config that launches nothing to start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<ProductSessionConfig>,

    /// Generic product information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<ProductInfoConfig>,

    /// The file paths to various build information.
    #[walk_paths]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_info: Option<BuildInfoConfig>,

    /// The policy given to component_manager that restricts where sensitive capabilities can be
    /// routed.
    #[walk_paths]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub component_policy: ComponentPolicyConfig,

    /// Components which depend on trusted applications running in the TEE.
    #[walk_paths]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tee_clients: Vec<TeeClient>,

    /// Components which should run as trusted applications in Fuchsia.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted_apps: Vec<TrustedApp>,

    /// A package that includes files to include in bootfs.
    ///
    /// This is only usable in the empty, embeddable, and bootstrap feature set levels.
    #[walk_paths]
    #[schemars(schema_with = "crate::option_path_schema")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootfs_files_package: Option<Utf8PathBuf>,
}

/// Packages provided by the product, to add to the assembled images.
///
/// This also includes configuration for those packages:
///
/// ```json5
///   packages: {
///     base: {
///       package_a: {
///         manifest: "path/to/package_a/package_manifest.json",
///       },
///       package_b: {
///         manifest: "path/to/package_b/package_manifest.json",
///         config_data: {
///           "foo.cfg": "path/to/some/source/file/foo.cfg",
///           "bar/more/data.json": "path/to/some.json",
///         },
///       },
///     ],
///     cache: []
///   }
/// ```
///
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(default, deny_unknown_fields, from = "ProductPackagesConfigDeserializeHelper")]
pub struct ProductPackagesConfig {
    /// Paths to package manifests, or more detailed json entries for packages
    /// to add to the 'base' package set, which are keyed by package name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub base: BTreeMap<String, ProductPackageDetails>,

    /// Paths to package manifests, or more detailed json entries for packages
    /// to add to the 'cache' package set, which are keyed by package name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub cache: BTreeMap<String, ProductPackageDetails>,

    /// Paths to package manifests, or more detailed json entries for packages
    /// to add to the 'bootfs' package set, which are keyed by package name.
    ///
    /// This is only usable in the empty, embeddable, and bootstrap feature set levels.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub bootfs: BTreeMap<String, ProductPackageDetails>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProductPackagesConfigDeserializeHelper {
    pub base: MapOrVecOfPackages,
    pub cache: MapOrVecOfPackages,
    pub bootfs: MapOrVecOfPackages,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MapOrVecOfPackages {
    Map(BTreeMap<String, ProductPackageDetails>),
    Vec(Vec<ProductPackageDetails>),
}

impl Default for MapOrVecOfPackages {
    fn default() -> Self {
        Self::Map(BTreeMap::default())
    }
}

fn convert_to_map(map_or_vec: MapOrVecOfPackages) -> BTreeMap<String, ProductPackageDetails> {
    match map_or_vec {
        MapOrVecOfPackages::Map(map) => map,
        // The key in the map defaults to the index in the vector.
        MapOrVecOfPackages::Vec(vec) => {
            vec.into_iter().enumerate().map(|(i, s)| (i.to_string(), s)).collect()
        }
    }
}

impl From<ProductPackagesConfigDeserializeHelper> for ProductPackagesConfig {
    fn from(helper: ProductPackagesConfigDeserializeHelper) -> Self {
        Self {
            base: convert_to_map(helper.base),
            cache: convert_to_map(helper.cache),
            bootfs: convert_to_map(helper.bootfs),
        }
    }
}

/// Describes in more detail a package to add to the assembly.
#[derive(Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProductPackageDetails {
    /// Path to the package manifest for this package.
    #[schemars(schema_with = "path_schema")]
    pub manifest: Utf8PathBuf,

    /// Map of config_data entries for this package, from the destination path
    /// within the package, to the path where the source file is to be found.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub config_data: Vec<ProductConfigData>,
}

fn walk_package_set<F>(
    set: &mut BTreeMap<String, ProductPackageDetails>,
    found: &mut F,
    dest: Utf8PathBuf,
) -> Result<()>
where
    F: WalkPathsFn,
{
    for (name, pkg) in set {
        let pkg_dest = dest.join(name);
        found(&mut pkg.manifest, pkg_dest.clone(), FileType::PackageManifest)?;

        // Add the config data so that it is identified by package name and destination.
        // This ensures that we do not have collisions between inputs with the same name.
        // For example: `{pkg_dest}/config_data/config.txt`
        for config in &mut pkg.config_data {
            let config_dest = pkg_dest.join("config_data").join(&config.destination);
            found(&mut config.source, config_dest, FileType::Unknown)?;
        }
    }
    Ok(())
}

impl WalkPaths for ProductPackagesConfig {
    fn walk_paths_with_dest<F: WalkPathsFn>(
        &mut self,
        found: &mut F,
        dest: Utf8PathBuf,
    ) -> anyhow::Result<()> {
        walk_package_set(&mut self.base, found, dest.join("base"))?;
        walk_package_set(&mut self.cache, found, dest.join("cache"))?;
        Ok(())
    }
}

impl From<PackageManifestPathBuf> for ProductPackageDetails {
    fn from(manifest: PackageManifestPathBuf) -> Self {
        let manifestpath: &Utf8Path = manifest.as_ref();
        let path: Utf8PathBuf = manifestpath.into();
        Self { manifest: path, config_data: Vec::default() }
    }
}

impl From<&str> for ProductPackageDetails {
    fn from(s: &str) -> Self {
        ProductPackageDetails { manifest: s.into(), config_data: Vec::default() }
    }
}

#[derive(Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProductConfigData {
    /// Path to the config file on the host.
    #[schemars(schema_with = "path_schema")]
    pub source: Utf8PathBuf,

    /// Path to find the file in the package on the target.
    pub destination: PackageInternalPathBuf,
}

/// Configuration options for product info.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProductInfoConfig {
    /// Name of the product.
    pub name: String,
    /// Model of the product.
    pub model: String,
    /// Manufacturer of the product.
    pub manufacturer: String,
}

/// Configuration options for build info.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, JsonSchema, WalkPaths)]
#[serde(deny_unknown_fields)]
pub struct BuildInfoConfig {
    /// Name of the product build target.
    pub name: String,
    /// Path to the version file.
    #[walk_paths]
    #[schemars(schema_with = "path_schema")]
    pub version: Utf8PathBuf,
    /// Path to the jiri snapshot.
    #[walk_paths]
    #[schemars(schema_with = "path_schema")]
    pub jiri_snapshot: Utf8PathBuf,
    /// Path to the latest commit date.
    #[walk_paths]
    #[schemars(schema_with = "path_schema")]
    pub latest_commit_date: Utf8PathBuf,
}

/// Configuration options for the component policy.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema, WalkPaths)]
#[serde(default, deny_unknown_fields)]
pub struct ComponentPolicyConfig {
    /// The file paths to a product-provided component policies.
    #[walk_paths]
    #[schemars(schema_with = "vec_path_schema")]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub product_policies: Vec<Utf8PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(default)]
pub struct TeeClientFeatures {
    /// Whether this component needs /dev-class/securemem routed to it. If true, the securemem
    //// directory will be routed as dev-securemem.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub securemem: bool,
    /// Whether this component requires persistent storage, routed as /data.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub persistent_storage: bool,
    /// Whether this component requires tmp storage, routed as /tmp.
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub tmp_storage: bool,
}

/// A configuration for a component which depends on TEE-based protocols.
/// Examples include components which implement DRM, or authentication services.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, WalkPaths, PartialEq)]
pub struct TeeClient {
    /// The URL of the component.
    pub component_url: String,
    /// GUIDs which of the form fuchsia.tee.Application.{GUID} will match a
    /// protocol provided by the TEE.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guids: Vec<String>,
    /// Capabilities provided by this component which should be routed to the
    /// rest of the system.
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Additional protocols which are required for this component to work, and
    /// which will be routed from 'parent'
    #[serde(default)]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub additional_required_protocols: Vec<String>,
    /// Config data files required for this component to work, and which will be inserted into
    /// config data for this package (with a package name based on the component URL)
    #[serde(default)]
    #[walk_paths]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_data: Option<TeeClientConfigData>,
    /// Additional features required for the component to function.
    #[serde(default)]
    #[serde(skip_serializing_if = "crate::common::is_default")]
    pub additional_required_features: TeeClientFeatures,
}

/// The map of config data files for the TeeClient.
///
/// We have to wrap the BTreeMap in order to be able to implement JsonSchema
/// for a Utf8PathBuf.
#[derive(Clone, Debug, Deserialize, Serialize, WalkPaths, PartialEq)]
pub struct TeeClientConfigData {
    /// The inner map of config data files.
    #[serde(flatten)]
    #[walk_paths]
    pub files: BTreeMap<String, Utf8PathBuf>,
}

impl JsonSchema for TeeClientConfigData {
    fn schema_name() -> String {
        "TeeClientConfigData".into()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let mut schema: schemars::schema::SchemaObject =
            <BTreeMap<String, String>>::json_schema(gen).into();
        schema.format = Some("BTreeMap<String, Utf8PathBuf>".to_owned());
        schema.into()
    }
}

/// Configuration for how to run a trusted application in Fuchsia.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
pub struct TrustedApp {
    /// The URL of the component.
    pub component_url: String,
    /// The GUID that identifies this trusted app for clients.
    pub guid: String,
}

/// Product configuration options for the session:
///
/// ```json5
///   session: {
///     url: "fuchsia-pkg://fuchsia.com/my_session#meta/my_session.cm",
///     initial_element: {
///         collection: "elements",
///         url: "fuchsia-pkg://fuchsia.com/my_component#meta/my_component.cm"
///         view_id_annotation: "my_component"
///     }
///   }
/// ```
///
#[derive(Debug, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProductSessionConfig {
    /// Start URL to pass to `session_manager`.
    pub url: String,

    /// Specifies initial element properties for the window manager.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_element: Option<InitialElement>,
}

/// Platform configuration options for the window manager.
#[derive(Debug, Deserialize, Serialize, PartialEq, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitialElement {
    /// Specifies the collection in which the window manager should launch the
    /// initial element, if one is given by `url`. Defaults to "elements".
    #[serde(default = "collection_default")]
    pub collection: String,

    /// Specifies the Fuchsia package URL of the element the window manager
    /// should launch on startup, if one is given.
    pub url: String,

    /// Specifies the annotation value by which the window manager can identify
    /// a view presented by the element it launched on startup, if one is given
    /// by `url`.
    pub view_id_annotation: String,
}

fn collection_default() -> String {
    String::from("elements")
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_util as util;

    #[test]
    fn test_default_serialization() {
        crate::common::tests::default_serialization_helper::<ProductConfig>();
    }

    #[test]
    fn test_product_provided_config_data() {
        let json5 = r#"
            {
                base: [
                    {
                        manifest: "path/to/base/package_manifest.json"
                    },
                    {
                        manifest: "some/other/manifest.json",
                        config_data: [
                            {
                                destination: "dest/path/cfg.txt",
                                source: "source/path/cfg.txt",
                            },
                            {
                                destination: "other_data.json",
                                source: "source_other_data.json",
                            },
                        ]
                    }
                ],
                cache: [
                    {
                        manifest: "path/to/cache/package_manifest.json"
                    }
                ]
            }
        "#;

        let mut cursor = std::io::Cursor::new(json5);
        let packages: ProductPackagesConfig = util::from_reader(&mut cursor).unwrap();
        assert_eq!(
            packages.base,
            [
                (
                    "0".to_string(),
                    ProductPackageDetails {
                        manifest: "path/to/base/package_manifest.json".into(),
                        config_data: Vec::default()
                    }
                ),
                (
                    "1".to_string(),
                    ProductPackageDetails {
                        manifest: "some/other/manifest.json".into(),
                        config_data: vec![
                            ProductConfigData {
                                destination: "dest/path/cfg.txt".into(),
                                source: "source/path/cfg.txt".into(),
                            },
                            ProductConfigData {
                                destination: "other_data.json".into(),
                                source: "source_other_data.json".into(),
                            },
                        ]
                    }
                ),
            ]
            .into()
        );
        assert_eq!(
            packages.cache,
            [(
                "0".to_string(),
                ProductPackageDetails {
                    manifest: "path/to/cache/package_manifest.json".into(),
                    config_data: Vec::default()
                }
            )]
            .into()
        );
    }

    #[test]
    fn product_packages_config_deserialization() {
        let from_list = r#"
            {
                base: [{
                    manifest: "some/other/manifest.json",
                    config_data: [
                        {
                            destination: "dest/path/cfg.txt",
                            source: "source/path/cfg.txt",
                        },
                        {
                            destination: "other_data.json",
                            source: "source_other_data.json",
                        },
                    ]
                }]
            }
        "#;
        let from_map = r#"
            {
                base: {
                    "0": {
                        manifest: "some/other/manifest.json",
                        config_data: [
                            {
                                destination: "dest/path/cfg.txt",
                                source: "source/path/cfg.txt",
                            },
                            {
                                destination: "other_data.json",
                                source: "source_other_data.json",
                            },
                        ]
                    }
                }
            }
        "#;
        let expected = ProductPackagesConfig {
            base: [(
                "0".to_string(),
                ProductPackageDetails {
                    manifest: "some/other/manifest.json".into(),
                    config_data: vec![
                        ProductConfigData {
                            destination: "dest/path/cfg.txt".into(),
                            source: "source/path/cfg.txt".into(),
                        },
                        ProductConfigData {
                            destination: "other_data.json".into(),
                            source: "source_other_data.json".into(),
                        },
                    ],
                },
            )]
            .into(),
            cache: BTreeMap::default(),
            bootfs: BTreeMap::default(),
        };

        let mut cursor = std::io::Cursor::new(from_list);
        let details: ProductPackagesConfig = util::from_reader(&mut cursor).unwrap();
        assert_eq!(details, expected);

        let mut cursor = std::io::Cursor::new(from_map);
        let details: ProductPackagesConfig = util::from_reader(&mut cursor).unwrap();
        assert_eq!(details, expected);
    }

    #[test]
    fn product_package_details_deserialization() {
        let json5 = r#"
            {
                manifest: "some/other/manifest.json",
                config_data: [
                    {
                        destination: "dest/path/cfg.txt",
                        source: "source/path/cfg.txt",
                    },
                    {
                        destination: "other_data.json",
                        source: "source_other_data.json",
                    },
                ]
            }
        "#;
        let expected = ProductPackageDetails {
            manifest: "some/other/manifest.json".into(),
            config_data: vec![
                ProductConfigData {
                    destination: "dest/path/cfg.txt".into(),
                    source: "source/path/cfg.txt".into(),
                },
                ProductConfigData {
                    destination: "other_data.json".into(),
                    source: "source_other_data.json".into(),
                },
            ],
        };
        let mut cursor = std::io::Cursor::new(json5);
        let details: ProductPackageDetails = util::from_reader(&mut cursor).unwrap();
        assert_eq!(details, expected);
    }

    #[test]
    fn product_package_details_serialization() {
        let entries = vec![
            ProductPackageDetails {
                manifest: "path/to/manifest.json".into(),
                config_data: Vec::default(),
            },
            ProductPackageDetails {
                manifest: "another/path/to/a/manifest.json".into(),
                config_data: vec![
                    ProductConfigData {
                        destination: "dest/path/A".into(),
                        source: "source/path/A".into(),
                    },
                    ProductConfigData {
                        destination: "dest/path/B".into(),
                        source: "source/path/B".into(),
                    },
                ],
            },
        ];
        let serialized = serde_json::to_value(entries).unwrap();
        let expected = serde_json::json!(
            [
                {
                    "manifest": "path/to/manifest.json"
                },
                {
                    "manifest": "another/path/to/a/manifest.json",
                    "config_data": [
                        {
                            "destination": "dest/path/A",
                            "source": "source/path/A",
                        },
                        {
                            "destination": "dest/path/B",
                            "source": "source/path/B",
                        },
                    ]
                }
            ]
        );
        assert_eq!(serialized, expected);
    }
}
