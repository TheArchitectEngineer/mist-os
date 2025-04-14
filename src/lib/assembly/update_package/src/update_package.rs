// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ::update_package::{ImageMetadata, ImagePackagesManifest};
use anyhow::{anyhow, bail, Context, Result};
use assembly_blob_size::BlobSizeCalculator;
use assembly_images_config::BlobfsLayout;
use assembly_manifest::{AssemblyManifest, Image};
use assembly_partitions_config::{PartitionsConfig, RecoveryStyle};
use assembly_tool::ToolProvider;
use assembly_update_packages_manifest::UpdatePackagesManifest;
use camino::{Utf8Path, Utf8PathBuf};
use epoch::EpochFile;
use fuchsia_merkle::Hash;
use fuchsia_pkg::{PackageBuilder, PackageManifest};
use fuchsia_url::{PinnedAbsolutePackageUrl, RepositoryUrl};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use utf8_path::PathToStringExt;

/// Maximum size of 200 KiB.
const UPDATE_PACKAGE_BUDGET: u64 = 200 * 1024 * 1024;

/// The result of the builder.
pub struct UpdatePackage {
    /// The merkle-root of the update package.
    pub merkle: Hash,

    /// The package manifests corresponding to all the packages built for the update.
    pub package_manifests: Vec<PackageManifest>,
}

/// A builder that constructs update packages.
pub struct UpdatePackageBuilder {
    /// Root name of the UpdatePackage and its associated images packages.
    /// This is typically only modified for OTA tests so that multiple UpdatePackages can be
    /// published to the same repository.
    name: String,

    /// Mapping of physical partitions to images.
    partitions: PartitionsConfig,

    /// Name of the board.
    /// Fuchsia confirms the board name matches before applying an update.
    board_name: String,

    /// Version of the update.
    version_file: PathBuf,

    /// The epoch of the system.
    /// Fuchsia confirms that the epoch changes in increasing order before applying an update.
    epoch: EpochFile,

    /// Images to update for a particular slot, such as the ZBI or VBMeta for SlotA.
    /// Currently, the UpdatePackage does not support both A and B slots.
    slot_primary: Option<Slot>,
    slot_recovery: Option<Slot>,

    /// Manifest of packages to include in the update.
    packages: UpdatePackagesManifest,

    /// The repository to use for the images packages.
    repository: RepositoryUrl,

    /// Directory to write outputs.
    outdir: Utf8PathBuf,

    /// Directory to write intermediate files.
    gendir: Utf8PathBuf,
}

/// A set of images to be updated in a particular slot.
pub enum Slot {
    /// A or B slots.
    Primary(AssemblyManifest),

    /// R slot.
    Recovery(AssemblyManifest),
}

impl Slot {
    /// Get the image manifest.
    fn manifest(&self) -> &AssemblyManifest {
        match self {
            Slot::Primary(m) => m,
            Slot::Recovery(m) => m,
        }
    }

    /// Get the (preferably signed) zbi and optional vbmeta, or None if no zbi image is present in
    /// this manifest.
    fn zbi_vbmeta_dtbo(
        &self,
    ) -> Option<(ImageMapping, Option<ImageMapping>, Option<ImageMapping>)> {
        let mut zbi = None;
        let mut vbmeta = None;
        let mut dtbo = None;

        for image in &self.manifest().images {
            match image {
                Image::ZBI { path: _, signed } => {
                    if *signed || zbi.is_none() {
                        zbi = Some(ImageMapping::new(image.source(), "zbi"));
                    }
                }
                Image::VBMeta(_) => {
                    vbmeta = Some(ImageMapping::new(image.source(), "vbmeta"));
                }
                Image::Dtbo(_) => {
                    dtbo = Some(ImageMapping::new(image.source(), "dtbo"));
                }
                _ => {}
            }
        }

        match zbi {
            Some(zbi) => Some((zbi, vbmeta, dtbo)),
            None => None,
        }
    }
}

/// A mapping between an image source path on host to the destination in an UpdatePackage.
struct ImageMapping {
    source: Utf8PathBuf,
    destination: String,
}

impl ImageMapping {
    /// Create a new Image Mapping from |source | to |destination|.
    fn new(source: impl Into<Utf8PathBuf>, destination: impl AsRef<str>) -> Self {
        Self { source: source.into(), destination: destination.as_ref().to_string() }
    }

    fn metadata(&self, url: PinnedAbsolutePackageUrl) -> Result<ImageMetadata> {
        ImageMetadata::for_path(&self.source, url, self.destination.clone())
            .with_context(|| format!("Failed to read/hash {:?}", self.source))
    }
}

/// A PackageBuilder configured to build the update package or one of its subpackages.
struct SubpackageBuilder {
    package: PackageBuilder,
    package_name: String,
    far_path: Utf8PathBuf,
    repository: RepositoryUrl,
    gendir: Utf8PathBuf,
}

impl SubpackageBuilder {
    /// Build and publish an update package or one of its subpackages. Returns a merkle-pinned
    /// fuchsia-pkg:// URL for the package with the hostname set to "fuchsia.com".
    fn build(self) -> Result<(PinnedAbsolutePackageUrl, PackageManifest)> {
        let SubpackageBuilder { package: builder, package_name, far_path, repository, gendir } =
            self;

        let manifest = builder
            .build(&gendir, &far_path)
            .with_context(|| format!("Failed to build the {package_name} package"))?;

        let url = PinnedAbsolutePackageUrl::new(
            repository,
            manifest.package_path().name().clone(),
            Some(manifest.package_path().variant().clone()),
            manifest.hash(),
        );

        Ok((url, manifest))
    }
}

impl UpdatePackageBuilder {
    /// Construct a new UpdatePackageBuilder with the minimal requirements for an UpdatePackage.
    pub fn new(
        partitions: PartitionsConfig,
        board_name: impl AsRef<str>,
        version_file: impl AsRef<Path>,
        epoch: EpochFile,
        outdir: impl AsRef<Utf8Path>,
    ) -> Self {
        Self {
            name: "update".into(),
            partitions,
            board_name: board_name.as_ref().into(),
            version_file: version_file.as_ref().to_path_buf(),
            epoch,
            slot_primary: None,
            slot_recovery: None,
            packages: UpdatePackagesManifest::default(),
            repository: RepositoryUrl::parse_host("fuchsia.com".to_string())
                .expect("valid host from static string"),
            outdir: outdir.as_ref().to_path_buf(),
            gendir: outdir.as_ref().to_path_buf(),
        }
    }

    /// Set the name of the UpdatePackage.
    pub fn set_name(&mut self, name: impl AsRef<str>) {
        self.name = name.as_ref().to_string();
    }

    /// Set the directory for writing intermediate files.
    pub fn set_gendir(&mut self, gendir: impl Into<Utf8PathBuf>) {
        self.gendir = gendir.into();
    }

    /// Update the images in |slot|.
    pub fn add_slot_images(&mut self, slot: Slot) {
        match slot {
            Slot::Primary(_) => self.slot_primary = Some(slot),
            Slot::Recovery(_) => self.slot_recovery = Some(slot),
        }
    }

    /// Add |packages| to the update.
    pub fn add_packages(&mut self, packages: UpdatePackagesManifest) {
        self.packages.append(packages);
    }

    /// Start building an update package or one of its subpackages, performing the steps that
    /// are common to all update packages.
    fn make_subpackage_builder(&self, subname: &str) -> Result<SubpackageBuilder> {
        let suffix = match subname {
            "" => subname.to_owned(),
            _ => format!("_{subname}"),
        };

        // It's not totally clear what the ABI revision means for the update
        // package. It isn't actually checked as part of the update process.
        // Maybe it should be - that way we could ensure that devices only apply
        // update packages they know they understand (currently, those checks
        // happen at a different layer that predates ABI revisions).
        //
        // If the ABI stamp *was* checked as part of the update process, we'd
        // have to be very deliberate about choosing which API level to target,
        // based on which versions of the OS we need to be able to consume the
        // update package.
        //
        // We'll set it to `INVALID` and decide on a more appropriate ABI
        // revision if/when we decide to check it. Any checks on the `INVALID`
        // ABI revision will fail, so this will hopefully ensure we don't
        // accidentally add any checks without the necessary care.
        //
        // TODO(https://fxbug.dev/328812629): Clarify what this means.
        let abi_revision = version_history::AbiRevision::INVALID;

        // The update package needs to be named 'update' to be accepted by the
        // `system-updater`.  Follow that convention for images packages as well.
        let package_name = format!("update{suffix}");
        let mut builder = PackageBuilder::new(&package_name, abi_revision);

        // However, they can have different published names.  And the name here
        // is the name to publish it under (and to include in the generated
        // package manifest).
        let base_publish_name = &self.name;
        let publish_name = format!("{base_publish_name}{suffix}");
        builder.published_name(publish_name);

        // Export the package's package manifest to paths that don't change
        // based on the configured publishing name.
        let manifest_path = self.outdir.join(format!("update{suffix}_package_manifest.json"));
        builder.manifest_path(&manifest_path);

        let far_path = self.outdir.join(format!("{package_name}.far"));
        let gendir = self.gendir.join(&package_name);

        Ok(SubpackageBuilder {
            package: builder,
            package_name,
            far_path,
            repository: self.repository.clone(),
            gendir,
        })
    }

    /// Set a custom repository to use when building the images packages.
    pub fn set_repository(&mut self, repository: RepositoryUrl) {
        self.repository = repository;
    }

    /// Build the update package and associated update images packages.
    pub fn build(self, tools: Box<dyn ToolProvider>) -> Result<UpdatePackage> {
        use serde_json::to_string;

        // Keep track of all the packages that were built, so that they can be returned.
        let mut package_manifests = vec![];

        // Keep track of the firmware images.
        struct FirmwareImage {
            source: Utf8PathBuf,
            destination: String,
            firmware_type: String,
        }
        let mut firmware_images = Vec::<FirmwareImage>::new();

        let mut assembly_manifest = ImagePackagesManifest::builder();

        // Generate the update_images_fuchsia package.
        let mut builder = self.make_subpackage_builder("images_fuchsia")?;
        if let Some(slot) = &self.slot_primary {
            let (zbi, vbmeta, dtbo) = slot
                .zbi_vbmeta_dtbo()
                .ok_or_else(|| anyhow!("primary slot missing a zbi image"))?;

            builder.package.add_file_as_blob(&zbi.destination, &zbi.source)?;

            if let Some(vbmeta) = &vbmeta {
                builder.package.add_file_as_blob(&vbmeta.destination, &vbmeta.source)?;
            }
            if let Some(dtbo) = &dtbo {
                firmware_images.push(FirmwareImage {
                    source: dtbo.source.clone(),
                    destination: dtbo.destination.clone(),
                    firmware_type: dtbo.destination.clone(),
                });
            }

            let (url, manifest) = builder.build()?;
            package_manifests.push(manifest);
            assembly_manifest.fuchsia_package(
                zbi.metadata(url.clone())?,
                vbmeta.map(|vbmeta| vbmeta.metadata(url)).transpose()?,
            );
        } else {
            let (_, manifest) = builder.build()?;
            package_manifests.push(manifest);
        }

        // Generate the update_images_recovery package.
        let mut builder = self.make_subpackage_builder("images_recovery")?;
        if let Some(slot) = &self.slot_recovery {
            let (zbi, vbmeta, _) = slot
                .zbi_vbmeta_dtbo()
                .ok_or_else(|| anyhow!("recovery slot missing a zbi image"))?;

            match self.partitions.recovery_style()? {
                RecoveryStyle::AB => {
                    firmware_images.push(FirmwareImage {
                        source: zbi.source,
                        destination: "recovery_zbi".into(),
                        firmware_type: "recovery_zbi".into(),
                    });
                    if let Some(vbmeta) = vbmeta {
                        firmware_images.push(FirmwareImage {
                            source: vbmeta.source,
                            destination: "recovery_vbmeta".into(),
                            firmware_type: "recovery_vbmeta".into(),
                        });
                    }
                    let (_url, manifest) = builder.build()?;
                    package_manifests.push(manifest);
                }
                RecoveryStyle::R => {
                    builder.package.add_file_as_blob(&zbi.destination, &zbi.source)?;

                    if let Some(vbmeta) = &vbmeta {
                        builder.package.add_file_as_blob(&vbmeta.destination, &vbmeta.source)?;
                    }

                    let (url, manifest) = builder.build()?;
                    package_manifests.push(manifest);

                    assembly_manifest.recovery_package(
                        zbi.metadata(url.clone())?,
                        vbmeta.map(|vbmeta| vbmeta.metadata(url)).transpose()?,
                    );
                }
                RecoveryStyle::NoRecovery => {
                    bail!("Has recovery images but no recovery partitions");
                }
            }
        } else {
            let (_, manifest) = builder.build()?;
            package_manifests.push(manifest);
        }

        for bootloader in &self.partitions.bootloader_partitions {
            let destination = match bootloader.partition_type.as_str() {
                "" => "firmware".to_string(),
                t => format!("firmware_{}", t),
            };
            firmware_images.push(FirmwareImage {
                source: bootloader.image.clone(),
                destination,
                firmware_type: bootloader.partition_type.clone(),
            });
        }

        // Generate the update_images_firmware package.
        let mut builder = self.make_subpackage_builder("images_firmware")?;
        if !firmware_images.is_empty() {
            let mut firmware = BTreeMap::new();

            for FirmwareImage { source, destination, .. } in &firmware_images {
                builder.package.add_file_as_blob(destination, source)?;
            }

            let (url, manifest) = builder.build()?;
            package_manifests.push(manifest);

            for FirmwareImage { source, destination, firmware_type } in &firmware_images {
                firmware.insert(
                    firmware_type.clone(),
                    ImageMetadata::for_path(&source, url.clone(), destination.clone())
                        .with_context(|| format!("Failed to read/hash {:?}", &source))?,
                );
            }

            assembly_manifest.firmware_package(firmware);
        } else {
            let (_, manifest) = builder.build()?;
            package_manifests.push(manifest);
        }

        let assembly_manifest = assembly_manifest.build();

        // Generate the update package itself.
        let mut builder = self.make_subpackage_builder("")?;
        builder.package.add_contents_as_blob(
            "packages.json",
            to_string(&self.packages)?,
            &self.gendir,
        )?;
        builder.package.add_contents_as_blob(
            "images.json",
            to_string(&assembly_manifest)?,
            &self.gendir,
        )?;
        builder.package.add_contents_as_blob(
            "epoch.json",
            to_string(&self.epoch)?,
            &self.gendir,
        )?;
        builder.package.add_contents_as_blob("board", &self.board_name, &self.gendir)?;
        builder.package.add_file_as_blob("version", self.version_file.path_to_string()?)?;
        let (_, manifest) = builder.build()?;

        // Ensure the update package is within size budget.
        // We use the worse layout for compression just in case.
        let blob_size_calculator = BlobSizeCalculator::new(tools, BlobfsLayout::DeprecatedPadded);
        let manifest_path = self.outdir.join("update_package_manifest.json");
        let blobs = blob_size_calculator
            .calculate(&vec![&manifest_path])
            .context("Calculating update package blob sizes")?;
        let mut total: u64 = 0;
        for blob in blobs {
            total += blob.size;
        }
        if total > UPDATE_PACKAGE_BUDGET {
            bail!(
                "Update package is over budget\nbudget: {}\nactual: {}",
                UPDATE_PACKAGE_BUDGET,
                total
            );
        }

        let merkle = manifest.hash();
        package_manifests.push(manifest);

        Ok(UpdatePackage { merkle, package_manifests })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_partitions_config::{BootloaderPartition, Partition, Slot as PartitionSlot};
    use assembly_tool::testing::{blobfs_side_effect, FakeToolProvider};
    use assembly_util::write_json_file;
    use fuchsia_archive::Utf8Reader;
    use fuchsia_hash::HASH_SIZE;
    use fuchsia_pkg::{MetaContents, PackagePath};
    use serde_json::json;
    use std::fs::File;
    use std::io::{BufReader, Write};
    use std::str::FromStr;
    use tempfile::{tempdir, NamedTempFile};
    use update_package::images::{self, AssetType, VersionedImagePackagesManifest};

    #[test]
    fn build() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let fake_bootloader_tmp = NamedTempFile::new().unwrap();
        let fake_bootloader = Utf8Path::from_path(fake_bootloader_tmp.path()).unwrap();

        let partitions_config = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![BootloaderPartition {
                partition_type: "tpl".into(),
                name: Some("firmware_tpl".into()),
                image: fake_bootloader.to_path_buf(),
            }],
            partitions: vec![Partition::ZBI {
                name: "zircon_a".into(),
                slot: PartitionSlot::A,
                size: None,
            }],
            hardware_revision: "hw".into(),
        };
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir,
        );

        // Add a ZBI to the update.
        let fake_zbi_tmp = NamedTempFile::new().unwrap();
        let fake_zbi = Utf8Path::from_path(fake_zbi_tmp.path()).unwrap();

        builder.add_slot_images(Slot::Primary(AssemblyManifest {
            images: vec![Image::ZBI { path: fake_zbi.to_path_buf(), signed: true }],
            board_name: "my_board".into(),
        }));

        builder.set_repository(RepositoryUrl::parse_host("test.com".to_string()).unwrap());
        let tool_provider = Box::new(FakeToolProvider::new_with_side_effect(blobfs_side_effect));
        builder.build(tool_provider).unwrap();

        let file = File::open(outdir.join("images.json")).unwrap();
        let reader = BufReader::new(file);
        let i: VersionedImagePackagesManifest = serde_json::from_reader(reader).unwrap();
        match i {
            VersionedImagePackagesManifest::Version1(v) => {
                assert_eq!(v.assets.len(), 1);
                let asset = &v.assets[0];
                assert_eq!(asset.slot, images::Slot::Fuchsia);
                assert_eq!(asset.type_, AssetType::Zbi);
                assert_eq!(asset.size, 0);

                assert_eq!(v.firmware.len(), 1);
                let firmware = &v.firmware[0];
                assert_eq!(firmware.type_, "tpl".to_string());
                assert_eq!(firmware.size, 0);
            }
        }

        let file = File::open(outdir.join("packages.json")).unwrap();
        let reader = BufReader::new(file);
        let p: UpdatePackagesManifest = serde_json::from_reader(reader).unwrap();
        assert_eq!(UpdatePackagesManifest::default(), p);

        let file = File::open(outdir.join("epoch.json")).unwrap();
        let reader = BufReader::new(file);
        let e: EpochFile = serde_json::from_reader(reader).unwrap();
        assert_eq!(epoch, e);

        let b = std::fs::read_to_string(outdir.join("board")).unwrap();
        assert_eq!("board", b);

        // Read the output and ensure it contains the right files (and their hashes).
        let far_path = outdir.join("update.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let mut contents: Vec<String> = contents.into_contents().into_keys().collect();
        contents.sort();
        let expected_contents: Vec<String> =
            vec!["board", "epoch.json", "images.json", "packages.json", "version"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.join("update_images_fuchsia.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_fuchsia","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let contents: Vec<String> = contents.into_contents().into_keys().collect();
        let expected_contents: Vec<String> = vec!["zbi".to_string()];
        assert_eq!(expected_contents, contents);

        let far_path = outdir.join("update_images_recovery.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_recovery","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let expected_contents = "\
        "
        .to_string();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.join("update_images_firmware.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_firmware","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let contents: Vec<String> = contents.into_contents().into_keys().collect();
        let expected_contents = vec!["firmware_tpl".to_string()];
        assert_eq!(expected_contents, contents);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.join("update.far").exists());
        assert!(outdir.join("update_package_manifest.json").exists());
        assert!(outdir.join("update_images_fuchsia.far").exists());
        assert!(outdir.join("update_images_recovery.far").exists());
        assert!(outdir.join("update_images_firmware.far").exists());
        assert!(outdir.join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn build_full() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let fake_bootloader_tmp = NamedTempFile::new().unwrap();
        let fake_bootloader = Utf8Path::from_path(fake_bootloader_tmp.path()).unwrap();

        let partitions_config = PartitionsConfig {
            bootstrap_partitions: vec![],
            unlock_credentials: vec![],
            bootloader_partitions: vec![BootloaderPartition {
                partition_type: "tpl".into(),
                name: Some("firmware_tpl".into()),
                image: fake_bootloader.to_path_buf(),
            }],
            partitions: vec![
                Partition::ZBI { name: "zircon_a".into(), slot: PartitionSlot::A, size: None },
                Partition::Dtbo { name: "dtbo_a".into(), slot: PartitionSlot::A, size: None },
                Partition::ZBI { name: "zircon_r".into(), slot: PartitionSlot::R, size: None },
                Partition::ZBI { name: "vbmeta_r".into(), slot: PartitionSlot::R, size: None },
            ],
            hardware_revision: "hw".into(),
        };
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir,
        );

        // Add a ZBI to the update.
        let fake_zbi_tmp = NamedTempFile::new().unwrap();
        let fake_dtbo_tmp = NamedTempFile::new().unwrap();
        let fake_zbi = Utf8Path::from_path(fake_zbi_tmp.path()).unwrap();
        let fake_dtbo = Utf8Path::from_path(fake_dtbo_tmp.path()).unwrap();

        builder.add_slot_images(Slot::Primary(AssemblyManifest {
            images: vec![
                Image::ZBI { path: fake_zbi.to_path_buf(), signed: true },
                Image::Dtbo(fake_dtbo.to_path_buf()),
            ],
            board_name: "my_board".into(),
        }));

        // Add a Recovery ZBI/VBMeta to the update.
        let fake_recovery_zbi_tmp = NamedTempFile::new().unwrap();
        let fake_recovery_zbi = Utf8Path::from_path(fake_recovery_zbi_tmp.path()).unwrap();

        let fake_recovery_vbmeta_tmp = NamedTempFile::new().unwrap();
        let fake_recovery_vbmeta = Utf8Path::from_path(fake_recovery_vbmeta_tmp.path()).unwrap();

        builder.add_slot_images(Slot::Recovery(AssemblyManifest {
            images: vec![
                Image::ZBI { path: fake_recovery_zbi.to_path_buf(), signed: true },
                Image::VBMeta(fake_recovery_vbmeta.to_path_buf()),
            ],
            board_name: "my_board".into(),
        }));

        // Build and ensure the output is correct.
        builder.set_repository(RepositoryUrl::parse_host("test.com".to_string()).unwrap());
        let tool_provider = Box::new(FakeToolProvider::new_with_side_effect(blobfs_side_effect));
        let update_package = builder.build(tool_provider).unwrap();
        assert_eq!(update_package.package_manifests.len(), 4);

        let file = File::open(outdir.join("images.json")).unwrap();
        let reader = BufReader::new(file);
        let i: VersionedImagePackagesManifest = serde_json::from_reader(reader).unwrap();
        match i {
            VersionedImagePackagesManifest::Version1(v) => {
                assert_eq!(v.assets.len(), 3);
                let asset = &v.assets[0];
                assert_eq!(asset.slot, images::Slot::Fuchsia);
                assert_eq!(asset.type_, AssetType::Zbi);
                assert_eq!(asset.size, 0);
                let asset = &v.assets[1];
                assert_eq!(asset.slot, images::Slot::Recovery);
                assert_eq!(asset.type_, AssetType::Zbi);
                assert_eq!(asset.size, 0);
                let asset = &v.assets[2];
                assert_eq!(asset.slot, images::Slot::Recovery);
                assert_eq!(asset.type_, AssetType::Vbmeta);
                assert_eq!(asset.size, 0);

                assert_eq!(v.firmware.len(), 2);
                let firmware = &v.firmware[0];
                assert_eq!(firmware.type_, "dtbo".to_string());
                assert_eq!(firmware.size, 0);
                let firmware = &v.firmware[1];
                assert_eq!(firmware.type_, "tpl".to_string());
                assert_eq!(firmware.size, 0);
            }
        }

        let file = File::open(outdir.join("packages.json")).unwrap();
        let reader = BufReader::new(file);
        let p: UpdatePackagesManifest = serde_json::from_reader(reader).unwrap();
        assert_eq!(UpdatePackagesManifest::default(), p);

        let file = File::open(outdir.join("epoch.json")).unwrap();
        let reader = BufReader::new(file);
        let e: EpochFile = serde_json::from_reader(reader).unwrap();
        assert_eq!(epoch, e);

        let b = std::fs::read_to_string(outdir.join("board")).unwrap();
        assert_eq!("board", b);

        // Read the output and ensure it contains the right files (and their hashes).
        let far_path = outdir.join("update.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let mut contents: Vec<String> = contents.into_contents().into_keys().collect();
        contents.sort();
        let expected_contents: Vec<String> =
            vec!["board", "epoch.json", "images.json", "packages.json", "version"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(expected_contents, contents);

        let far_path = outdir.join("update_images_fuchsia.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_fuchsia","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let contents: Vec<String> = contents.into_contents().into_keys().collect();
        let expected_contents = vec!["zbi".to_string()];
        assert_eq!(expected_contents, contents);

        let far_path = outdir.join("update_images_recovery.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_recovery","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let mut contents: Vec<String> = contents.into_contents().into_keys().collect();
        contents.sort();
        let expected_contents = vec!["vbmeta".to_string(), "zbi".to_string()];
        assert_eq!(expected_contents, contents);

        let far_path = outdir.join("update_images_firmware.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_firmware","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let mut contents: Vec<String> = contents.into_contents().into_keys().collect();
        contents.sort();
        let expected_contents = vec!["dtbo".to_string(), "firmware_tpl".to_string()];
        assert_eq!(expected_contents, contents);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.join("update.far").exists());
        assert!(outdir.join("update_package_manifest.json").exists());
        assert!(outdir.join("update_images_fuchsia.far").exists());
        assert!(outdir.join("update_images_recovery.far").exists());
        assert!(outdir.join("update_images_firmware.far").exists());
        assert!(outdir.join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn build_emits_empty_image_packages() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let partitions_config = PartitionsConfig::default();
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let builder = UpdatePackageBuilder::new(
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir,
        );

        let tool_provider = Box::new(FakeToolProvider::new_with_side_effect(blobfs_side_effect));
        builder.build(tool_provider).unwrap();

        // Ensure the generated images.json manifest is empty.
        let file = File::open(outdir.join("images.json")).unwrap();
        let reader = BufReader::new(file);
        let i: ::update_package::VersionedImagePackagesManifest =
            serde_json::from_reader(reader).unwrap();
        assert_eq!(ImagePackagesManifest::builder().build(), i);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.join("update.far").exists());
        assert!(outdir.join("update_package_manifest.json").exists());
        assert!(outdir.join("update_images_fuchsia.far").exists());
        assert!(outdir.join("update_images_recovery.far").exists());
        assert!(outdir.join("update_images_firmware.far").exists());
        assert!(outdir.join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn build_ab_recovery() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let partitions_config = PartitionsConfig {
            partitions: vec![
                Partition::RecoveryZBI {
                    name: "recovery_zbi_a".into(),
                    slot: PartitionSlot::A,
                    size: None,
                },
                Partition::RecoveryVBMeta {
                    name: "recovery_vbmeta_a".into(),
                    slot: PartitionSlot::A,
                    size: None,
                },
            ],
            ..PartitionsConfig::default()
        };
        let epoch = EpochFile::Version1 { epoch: 0 };
        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            partitions_config,
            "board",
            fake_version.path().to_path_buf(),
            epoch.clone(),
            &outdir,
        );

        // Add a Recovery ZBI/VBMeta to the update.
        let fake_recovery_zbi_tmp = NamedTempFile::new().unwrap();
        let fake_recovery_zbi = Utf8Path::from_path(fake_recovery_zbi_tmp.path()).unwrap();

        let fake_recovery_vbmeta_tmp = NamedTempFile::new().unwrap();
        let fake_recovery_vbmeta = Utf8Path::from_path(fake_recovery_vbmeta_tmp.path()).unwrap();

        builder.add_slot_images(Slot::Recovery(AssemblyManifest {
            images: vec![
                Image::ZBI { path: fake_recovery_zbi.to_path_buf(), signed: true },
                Image::VBMeta(fake_recovery_vbmeta.to_path_buf()),
            ],
            board_name: "my_board".into(),
        }));

        let tool_provider = Box::new(FakeToolProvider::new_with_side_effect(blobfs_side_effect));
        builder.build(tool_provider).unwrap();

        let file = File::open(outdir.join("images.json")).unwrap();
        let reader = BufReader::new(file);
        let i: VersionedImagePackagesManifest = serde_json::from_reader(reader).unwrap();
        match i {
            VersionedImagePackagesManifest::Version1(v) => {
                assert_eq!(v.assets.len(), 0);

                assert_eq!(v.firmware.len(), 2);
                let firmware = &v.firmware[0];
                assert_eq!(firmware.type_, "recovery_vbmeta".to_string());
                assert_eq!(firmware.size, 0);
                let firmware = &v.firmware[1];
                assert_eq!(firmware.type_, "recovery_zbi".to_string());
                assert_eq!(firmware.size, 0);
            }
        }

        let far_path = outdir.join("update_images_firmware.far");
        let mut far_reader = Utf8Reader::new(File::open(&far_path).unwrap()).unwrap();
        let package = far_reader.read_file("meta/package").unwrap();
        assert_eq!(package, br#"{"name":"update_images_firmware","version":"0"}"#);
        let contents = far_reader.read_file("meta/contents").unwrap();
        let contents = std::str::from_utf8(&contents).unwrap();
        let contents = MetaContents::deserialize(std::io::Cursor::new(contents)).unwrap();
        let mut contents: Vec<String> = contents.into_contents().into_keys().collect();
        contents.sort();
        let expected_contents = vec!["recovery_vbmeta".to_string(), "recovery_zbi".to_string()];
        assert_eq!(expected_contents, contents);

        // Ensure the expected package fars/manifests were generated.
        assert!(outdir.join("update.far").exists());
        assert!(outdir.join("update_package_manifest.json").exists());
        assert!(outdir.join("update_images_fuchsia.far").exists());
        assert!(outdir.join("update_images_recovery.far").exists());
        assert!(outdir.join("update_images_firmware.far").exists());
        assert!(outdir.join("update_images_fuchsia_package_manifest.json").exists());
        assert!(outdir.join("update_images_recovery_package_manifest.json").exists());
        assert!(outdir.join("update_images_firmware_package_manifest.json").exists());
    }

    #[test]
    fn name() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            PartitionsConfig::default(),
            "board",
            fake_version.path().to_path_buf(),
            EpochFile::Version1 { epoch: 0 },
            &outdir,
        );
        builder.set_name("update_2");
        let tool_provider = Box::new(FakeToolProvider::new_with_side_effect(blobfs_side_effect));
        assert!(builder.build(tool_provider).is_ok());

        // Read the package manifest and ensure it contains the updated name.
        let manifest_path = outdir.join("update_package_manifest.json");
        let manifest = PackageManifest::try_load_from(manifest_path).unwrap();
        assert_eq!("update_2", manifest.name().as_ref());
    }

    #[test]
    fn over_budget() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            PartitionsConfig::default(),
            "board",
            fake_version.path().to_path_buf(),
            EpochFile::Version1 { epoch: 0 },
            &outdir,
        );
        builder.set_name("update_2");
        let tool_provider =
            Box::new(FakeToolProvider::new_with_side_effect(|_name: &str, args: &[String]| {
                assert_eq!(args[0], "--json-output");
                write_json_file(
                    Path::new(&args[1]),
                    &json!([{
                      "merkle": "b62ee413090825c2ae70fe143b34cbd851f055932cfd5e7ca4ef0efbb802da2a",
                      "size": UPDATE_PACKAGE_BUDGET + 1,
                    }]),
                )
                .unwrap();
            }));
        assert!(builder.build(tool_provider).is_err());
    }

    #[test]
    fn packages() {
        let tmp = tempdir().unwrap();
        let outdir = Utf8Path::from_path(tmp.path()).unwrap();

        let mut fake_version = NamedTempFile::new().unwrap();
        writeln!(fake_version, "1.2.3.4").unwrap();
        let mut builder = UpdatePackageBuilder::new(
            PartitionsConfig::default(),
            "board",
            fake_version.path().to_path_buf(),
            EpochFile::Version1 { epoch: 0 },
            &outdir,
        );

        let hash = Hash::from([0u8; HASH_SIZE]);
        let mut list1 = UpdatePackagesManifest::default();
        list1.add(PackagePath::from_str("one/0").unwrap(), hash.clone(), None).unwrap();
        list1.add(PackagePath::from_str("two/0").unwrap(), hash.clone(), None).unwrap();
        builder.add_packages(list1);

        let mut list2 = UpdatePackagesManifest::default();
        list2.add(PackagePath::from_str("three/0").unwrap(), hash.clone(), None).unwrap();
        list2.add(PackagePath::from_str("four/0").unwrap(), hash.clone(), None).unwrap();
        builder.add_packages(list2);

        let tool_provider = Box::new(FakeToolProvider::new_with_side_effect(blobfs_side_effect));
        assert!(builder.build(tool_provider).is_ok());

        // Read the package list and ensure it contains the correct contents.
        let package_list_path = outdir.join("packages.json");
        let package_list_file = File::open(package_list_path).unwrap();
        let list3: UpdatePackagesManifest = serde_json::from_reader(package_list_file).unwrap();
        let UpdatePackagesManifest::V1(pkg_urls) = list3;
        let pkg1 = PinnedAbsolutePackageUrl::new(
            "fuchsia-pkg://fuchsia.com".parse().unwrap(),
            "one".parse().unwrap(),
            Some(fuchsia_url::PackageVariant::zero()),
            hash.clone(),
        );
        println!("pkg_urls={:?}", &pkg_urls);
        println!("pkg={:?}", pkg1);
        assert!(pkg_urls.contains(&pkg1));
    }
}
