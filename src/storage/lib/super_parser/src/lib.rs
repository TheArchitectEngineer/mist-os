// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod format;

use crate::format::{
    MetadataExtent, MetadataGeometry, MetadataHeader, METADATA_GEOMETRY_RESERVED_SIZE,
    PARTITION_RESERVED_BYTES,
};
use anyhow::{anyhow, ensure, Error};
use format::{
    MetadataBlockDevice, MetadataPartition, MetadataPartitionGroup, MetadataTableDescriptor,
    ValidateTable,
};
use sha2::Digest;
use storage_device::fake_device::FakeDevice;
use storage_device::Device;
use zerocopy::FromBytes;

fn round_up_to_alignment(x: u32, alignment: u32) -> Result<u32, Error> {
    ensure!(alignment.count_ones() == 1, "alignment should be a power of 2");
    alignment
        .checked_mul(x.div_ceil(alignment))
        .ok_or(anyhow!("overflow occurred when rounding up to nearest alignment"))
}

// TODO(https://fxbug.dev/404952286): Add checks for potential arithmetic overflows from data read
// from the device and test for this.
// TODO(https://fxbug.dev/404952286): Add fuzzer to check for arithmetic overflows.

/// Struct to help interpret the deserialized "super" metadata.
#[derive(Debug)]
pub struct Metadata {
    geometry: MetadataGeometry,
    header: MetadataHeader,
    partitions: Vec<MetadataPartition>,
    extents: Vec<MetadataExtent>,
    partition_groups: Vec<MetadataPartitionGroup>,
    block_devices: Vec<MetadataBlockDevice>,
}

impl Metadata {
    pub async fn load_from_device(device: &FakeDevice) -> Result<Self, Error> {
        let geometry = Self::load_metadata_geometry(&device).await?;

        let header = Self::parse_metadata_header(&device).await.unwrap();

        // Now that we have more information on the tables, validate table-specific fields.
        ensure!(header.tables_size <= geometry.metadata_max_size, "Invalid tables size.");

        // TODO(https://fxbug.dev/404952286): read backup metadata if we fail any of the validation
        // the first time.

        // Get tables bytes
        let header_offset = PARTITION_RESERVED_BYTES + 2 * METADATA_GEOMETRY_RESERVED_SIZE;
        ensure!(header_offset % device.block_size() == 0, "Reads must be block aligned.");
        let header_and_tables_size = header
            .header_size
            .checked_add(header.tables_size)
            .ok_or_else(|| anyhow!("arithmetic overflow: cannot calculate header and tables size to read from device"))?;
        let buffer_len = round_up_to_alignment(header_and_tables_size, device.block_size())?;
        let mut buffer = device.allocate_buffer(buffer_len as usize).await;
        device.read(header_offset as u64, buffer.as_mut()).await?;
        let tables_bytes =
            &buffer.as_slice()[header.header_size as usize..header_and_tables_size as usize];

        // Read tables to verify `table_checksum` in metadata_header.
        let computed_tables_checksum: [u8; 32] = sha2::Sha256::digest(tables_bytes).into();
        ensure!(
            computed_tables_checksum == header.tables_checksum,
            "Invalid metadata tables checksum"
        );

        // Parse partition table entries.
        let tables_offset = 0;
        let partitions = Self::parse_table::<MetadataPartition>(
            tables_bytes,
            tables_offset,
            &header,
            &header.partitions,
        )
        .await
        .unwrap();

        // Parse extent table entries.
        let tables_offset = tables_offset
            .checked_add(header.partitions.get_table_size()?)
            .ok_or_else(|| anyhow!("Adding offset + num_entries * entry_size overflowed."))?;
        let extents = Self::parse_table::<MetadataExtent>(
            tables_bytes,
            tables_offset,
            &header,
            &header.extents,
        )
        .await
        .unwrap();

        // Parse partition group table entries.
        let tables_offset = tables_offset
            .checked_add(header.extents.get_table_size()?)
            .ok_or_else(|| anyhow!("Adding offset + num_entries * entry_size overflowed."))?;
        let partition_groups = Self::parse_table::<MetadataPartitionGroup>(
            tables_bytes,
            tables_offset,
            &header,
            &header.groups,
        )
        .await
        .unwrap();

        // Parse block device table entries.
        let tables_offset = tables_offset
            .checked_add(header.groups.get_table_size()?)
            .ok_or_else(|| anyhow!("Adding offset + num_entries * entry_size overflowed."))?;
        let block_devices = Self::parse_table::<MetadataBlockDevice>(
            tables_bytes,
            tables_offset,
            &header,
            &header.block_devices,
        )
        .await
        .unwrap();

        // Expect there to be at least be one block device: "super".
        ensure!(block_devices.len() > 0, "Metadata did not specify a super device.");
        let super_device = block_devices[0];
        let logical_partition_offset = super_device.get_first_logical_sector_in_bytes()?;
        let metadata_region = geometry.get_total_metadata_size()?;
        ensure!(
            metadata_region <= logical_partition_offset,
            "Logical partition metadata overlaps with logical partition contents."
        );

        Ok(Self { geometry, header, partitions, extents, partition_groups, block_devices })
    }

    // Load and validate geometry information from a block device that holds logical partitions.
    async fn load_metadata_geometry(device: &FakeDevice) -> Result<MetadataGeometry, Error> {
        // Read the primary geometry
        match Self::parse_metadata_geometry(&device, PARTITION_RESERVED_BYTES).await {
            Ok(geometry) => Ok(geometry),
            Err(_) => {
                // Try the backup geometry
                Self::parse_metadata_geometry(
                    &device,
                    PARTITION_RESERVED_BYTES + METADATA_GEOMETRY_RESERVED_SIZE,
                )
                .await
            }
        }
    }

    // Read and validates the metadata geometry. The offset provided will be rounded up to the
    // nearest block alignment.
    async fn parse_metadata_geometry(
        device: &FakeDevice,
        offset: u32,
    ) -> Result<MetadataGeometry, Error> {
        let buffer_len =
            round_up_to_alignment(METADATA_GEOMETRY_RESERVED_SIZE, device.block_size())?;
        let mut buffer = device.allocate_buffer(buffer_len as usize).await;
        let aligned_offset = round_up_to_alignment(offset, device.block_size())?;
        device.read(aligned_offset as u64, buffer.as_mut()).await?;
        let full_buffer = buffer.as_slice();
        let (metadata_geometry, _remainder) =
            MetadataGeometry::read_from_prefix(full_buffer).unwrap();
        metadata_geometry.validate()?;
        Ok(metadata_geometry)
    }

    async fn parse_metadata_header(device: &FakeDevice) -> Result<MetadataHeader, Error> {
        // Reads must be block aligned.
        let metadata_header_offset = PARTITION_RESERVED_BYTES + 2 * METADATA_GEOMETRY_RESERVED_SIZE;
        ensure!(metadata_header_offset % device.block_size() == 0, "Reads must be block aligned.");
        let buffer_len = round_up_to_alignment(
            std::mem::size_of::<MetadataHeader>() as u32,
            device.block_size(),
        )?;
        let mut buffer = device.allocate_buffer(buffer_len as usize).await;
        device.read(metadata_header_offset as u64, buffer.as_mut()).await?;

        let full_buffer = buffer.as_slice();
        let (mut metadata_header, _remainder) =
            MetadataHeader::read_from_prefix(full_buffer).unwrap();
        // Validation will also check if the header is an older version, and if so, will zero the
        // fields in `MetadataHeader` that did not exist in the older version.
        metadata_header.validate()?;
        Ok(metadata_header)
    }

    async fn parse_table<T: ValidateTable + FromBytes>(
        tables_bytes: &[u8],
        tables_offset: u32,
        metadata_header: &MetadataHeader,
        table_descriptor: &MetadataTableDescriptor,
    ) -> Result<Vec<T>, Error> {
        let mut entries = Vec::new();
        let entry_size = table_descriptor.entry_size;
        let num_entries = table_descriptor.num_entries;
        for index in 0..num_entries {
            let read_so_far = index
                .checked_mul(entry_size)
                .ok_or_else(|| anyhow!("arithmetic overflow occurred"))?;
            let offset = tables_offset
                .checked_add(read_so_far)
                .ok_or_else(|| anyhow!("arithmetic overflow occurred"))?;
            let (entry, _remainder) =
                T::read_from_prefix(&tables_bytes[offset as usize..]).unwrap();
            entry.validate(metadata_header)?;
            entries.push(entry);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use crate::format::{PartitionAttributes, SECTOR_SIZE};

    use super::*;
    use std::path::Path;
    use storage_device::fake_device::FakeDevice;

    const BLOCK_SIZE: u32 = 4096;
    const IMAGE_PATH: &str = "/pkg/data/simple_super.img.zstd";

    fn open_image(path: &Path) -> FakeDevice {
        // If image changes at the file path, need to update the `verify_*` functions below that
        // verifies the parser.
        let file = std::fs::File::open(path).expect("open file failed");
        let image = zstd::Decoder::new(file).expect("decompress image failed");
        FakeDevice::from_image(image, BLOCK_SIZE).expect("create fake block device failed")
    }

    // Verify metadata geometry against the image at `IMAGE_PATH`.
    fn verify_geometry(metadata: &Metadata) -> Result<(), Error> {
        let geometry = metadata.geometry;
        let max_size = geometry.metadata_max_size;
        let metadata_slot_count = geometry.metadata_slot_count;
        assert_eq!(max_size, 65536);
        assert_eq!(metadata_slot_count, 2);
        Ok(())
    }

    // Verify metadata header against the image at `IMAGE_PATH`.
    fn verify_header(metadata: &Metadata) -> Result<(), Error> {
        let header = metadata.header;
        let num_partition_entries = header.partitions.num_entries;
        let num_extent_entries = header.extents.num_entries;
        assert_eq!(num_partition_entries, 2);
        assert_eq!(num_extent_entries, 2);
        Ok(())
    }

    // Verify metadata partitions table against the image at `IMAGE_PATH`.
    fn verify_partitions_table(metadata: &Metadata) -> Result<(), Error> {
        let partitions = &metadata.partitions;
        // The first entry in the partitions table is "system".
        let expected_name = "system".to_string();
        let partition_name = String::from_utf8(partitions[0].name.to_vec())
            .expect("failed to convert partition entry name to string");
        assert_eq!(partition_name[..expected_name.len()], expected_name);
        let partition_attributes = partitions[0].attributes;
        assert_eq!(partition_attributes, PartitionAttributes::READONLY);
        let system_partition_extent_index = partitions[0].first_extent_index;
        let system_partition_num_extents = partitions[0].num_extents;
        assert_eq!(system_partition_extent_index, 0);
        assert_eq!(system_partition_num_extents, 1);

        // The next entry in the partitions table is "system_ext".
        let expected_name = "system_ext".to_string();
        let partition_name = String::from_utf8(partitions[1].name.to_vec())
            .expect("failed to convert partition entry name to string");
        assert_eq!(partition_name[..expected_name.len()], expected_name);
        let partition_attributes = partitions[1].attributes;
        assert_eq!(partition_attributes, PartitionAttributes::READONLY);
        let system_partition_extent_index = partitions[1].first_extent_index;
        let system_partition_num_extents = partitions[1].num_extents;
        assert_eq!(system_partition_extent_index, 1);
        assert_eq!(system_partition_num_extents, 1);
        Ok(())
    }

    // Verify metadata extents table against the image at `IMAGE_PATH`.
    fn verify_extents_table(metadata: &Metadata) -> Result<(), Error> {
        // The simple super image has a "system" partition of size 8192 bytes. This extent entry
        // refers to the extent used by that partition.
        let num_sectors = metadata.extents[0].num_sectors;
        assert_eq!(num_sectors * SECTOR_SIZE as u64, 8192);

        // The simple super image has a "system_ext" partition of size 4096 bytes. This extent entry
        // refers to the extent used by that partition.
        let num_sectors = metadata.extents[1].num_sectors;
        assert_eq!(num_sectors * SECTOR_SIZE as u64, 4096);
        Ok(())
    }

    // Verify metadata partition groups table against the image at `IMAGE_PATH`.
    fn verify_partition_groups_table(metadata: &Metadata) -> Result<(), Error> {
        // Expect to see one group, "default", of unlimited maximum size.
        assert_eq!(metadata.partition_groups.len(), 1);
        let group = metadata.partition_groups[0];
        let expected_name = "default".to_string();
        let name = String::from_utf8(group.name.to_vec())
            .expect("failed to convert partition group name to string");
        assert_eq!(name[..expected_name.len()], expected_name);
        let maximum_size = group.maximum_size;
        assert_eq!(maximum_size, 0);
        Ok(())
    }

    // Verify metadata block devices table against the image at `IMAGE_PATH`.
    fn verify_block_devices_table(metadata: &Metadata) -> Result<(), Error> {
        let block_devices = &metadata.block_devices;
        let expected_name = "super".to_string();
        let partition_name = String::from_utf8(block_devices[0].partition_name.to_vec())
            .expect("failed to convert partition entry name to string");
        assert_eq!(partition_name[..expected_name.len()], expected_name);
        let device_size = block_devices[0].size;
        assert_eq!(device_size, 10485760);
        Ok(())
    }

    #[fuchsia::test]
    async fn test_parsing_metadata() {
        let device = open_image(std::path::Path::new(IMAGE_PATH));

        let super_partition =
            Metadata::load_from_device(&device).await.expect("failed to load super metatata.");

        verify_geometry(&super_partition).expect("incorrect geometry");
        verify_header(&super_partition).expect("incorrect header");
        verify_partitions_table(&super_partition).expect("incorrect partitions table");
        verify_extents_table(&super_partition).expect("incorrect extents table");
        verify_partition_groups_table(&super_partition).expect("incorrect partition groups table");
        verify_block_devices_table(&super_partition).expect("incorrect block devices table");
        device.close().await.expect("failed to close device");
    }

    #[fuchsia::test]
    async fn test_parsing_metadata_with_invalid_primary_geometry() {
        let device = open_image(std::path::Path::new(IMAGE_PATH));

        // Corrupt the primary geometry bytes.
        {
            let offset = round_up_to_alignment(PARTITION_RESERVED_BYTES, device.block_size())
                .expect("failed to round to nearest block");
            let buf_len =
                round_up_to_alignment(METADATA_GEOMETRY_RESERVED_SIZE, device.block_size())
                    .expect("failed to round to nearest block");
            let mut buf = device.allocate_buffer(buf_len as usize).await;
            buf.as_mut_slice().fill(0xaa as u8);
            device.write(offset as u64, buf.as_ref()).await.expect("failed to write to device");
        }

        let super_partition =
            Metadata::load_from_device(&device).await.expect("failed to load super metatata.");
        verify_geometry(&super_partition).expect("incorrect geometry");
    }

    #[fuchsia::test]
    async fn test_parsing_metadata_with_invalid_primary_and_backup_geometry() {
        let device = open_image(std::path::Path::new(IMAGE_PATH));

        // Corrupt the primary and backup geometry bytes.
        {
            let offset = round_up_to_alignment(PARTITION_RESERVED_BYTES, device.block_size())
                .expect("failed to round to nearest block");
            let buf_len =
                round_up_to_alignment(2 * METADATA_GEOMETRY_RESERVED_SIZE, device.block_size())
                    .expect("failed to round to nearest block");
            let mut buf = device.allocate_buffer(buf_len as usize).await;
            buf.as_mut_slice().fill(0xaa as u8);
            device.write(offset as u64, buf.as_ref()).await.expect("failed to write to device");
        }

        Metadata::load_from_device(&device)
            .await
            .expect_err("passed loading super metatata unexpectedly");
    }
}
