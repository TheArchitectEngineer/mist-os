// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lsm_tree::{PersistentLayerHeader, PersistentLayerInfo};
use crate::object_store::allocator::{AllocatorInfo, AllocatorKey, AllocatorValue};
use crate::object_store::journal::super_block::{
    SuperBlockHeader, SuperBlockRecord, SuperBlockRecordV40, SuperBlockRecordV41,
    SuperBlockRecordV43,
};
use crate::object_store::journal::{
    JournalRecord, JournalRecordV40, JournalRecordV41, JournalRecordV42, JournalRecordV43,
};
use crate::object_store::object_record::{
    FsverityMetadata, ObjectKey, ObjectKeyV40, ObjectValue, ObjectValueV40, ObjectValueV41,
};
use crate::object_store::transaction::{Mutation, MutationV40, MutationV41, MutationV43};
use crate::object_store::{EncryptedMutations, StoreInfo};
use crate::serialized_types::{versioned_type, Version, Versioned, VersionedLatest};

/// The latest version of on-disk filesystem format.
///
/// If all layer files are compacted the the journal flushed, and super-block
/// both rewritten, all versions should match this value.
///
/// If making a breaking change, please see EARLIEST_SUPPORTED_VERSION (below).
///
/// IMPORTANT: When changing this (major or minor), update the list of possible versions at
/// https://cs.opensource.google/fuchsia/fuchsia/+/main:third_party/cobalt_config/fuchsia/local_storage/versions.txt.
pub const LATEST_VERSION: Version = Version { major: 46, minor: 0 };

/// The earliest supported version of the on-disk filesystem format.
///
/// When a breaking change is made:
/// 1) LATEST_VERSION should have it's major component increased (see above).
/// 2) EARLIEST_SUPPORTED_VERSION should be set to the new LATEST_VERSION.
/// 3) The SuperBlockHeader version (below) should also be set to the new LATEST_VERSION.
///
/// Also check the constant version numbers above for any code cleanup that can happen.
pub const EARLIEST_SUPPORTED_VERSION: Version = Version { major: 40, minor: 0 };

/// From this version of the filesystem, we shrink the size of the extents that are reserved for
/// the superblock and root-parent store to a single block.
pub const SMALL_SUPERBLOCK_VERSION: Version = Version { major: 44, minor: 0 };

/// From this version of the filesystem, the superblock explicitly includes a record for it's
/// first extent. Prior to this, the first extent was assumed based on hard-coded location.
pub const FIRST_EXTENT_IN_SUPERBLOCK_VERSION: Version = Version { major: 45, minor: 0 };

versioned_type! {
    32.. => AllocatorInfo,
}
versioned_type! {
    32.. => AllocatorKey,
}
versioned_type! {
    32.. => AllocatorValue,
}
versioned_type! {
    40.. => EncryptedMutations,
}
versioned_type! {
    33.. => FsverityMetadata,
}
versioned_type! {
    46.. => JournalRecord,
    43.. => JournalRecordV43,
    42.. => JournalRecordV42,
    41.. => JournalRecordV41,
    40.. => JournalRecordV40,
}
versioned_type! {
    46.. => Mutation,
    43.. => MutationV43,
    41.. => MutationV41,
    40.. => MutationV40,
}
versioned_type! {
    43.. => ObjectKey,
    40.. => ObjectKeyV40,
}
versioned_type! {
    46.. => ObjectValue,
    41.. => ObjectValueV41,
    40.. => ObjectValueV40,
}
versioned_type! {
    39.. => PersistentLayerHeader,
}
versioned_type! {
    39.. => PersistentLayerInfo,
}
versioned_type! {
    40.. => StoreInfo,
}
versioned_type! {
    32.. => SuperBlockHeader,
}
versioned_type! {
    46.. => SuperBlockRecord,
    43.. => SuperBlockRecordV43,
    41.. => SuperBlockRecordV41,
    40.. => SuperBlockRecordV40,
}

#[cfg(test)]
mod tests {
    use crate::lsm_tree::{
        PersistentLayerHeader, PersistentLayerHeaderV39, PersistentLayerInfo,
        PersistentLayerInfoV39,
    };
    use crate::object_store::allocator::{
        AllocatorInfo, AllocatorInfoV32, AllocatorKey, AllocatorKeyV32, AllocatorValue,
        AllocatorValueV32,
    };
    use crate::object_store::journal::super_block::{
        SuperBlockHeader, SuperBlockHeaderV32, SuperBlockRecord, SuperBlockRecordV40,
        SuperBlockRecordV41, SuperBlockRecordV43, SuperBlockRecordV46,
    };
    use crate::object_store::journal::{
        JournalRecord, JournalRecordV40, JournalRecordV41, JournalRecordV42, JournalRecordV43,
        JournalRecordV46,
    };
    use crate::object_store::object_record::{
        FsverityMetadataV33, ObjectKey, ObjectKeyV40, ObjectKeyV43, ObjectValue, ObjectValueV40,
        ObjectValueV41, ObjectValueV46,
    };
    use crate::object_store::transaction::{MutationV40, MutationV41, MutationV43, MutationV46};
    use crate::object_store::{
        EncryptedMutations, EncryptedMutationsV40, FsverityMetadata, Mutation, StoreInfo,
        StoreInfoV40,
    };

    fn assert_type_fprint<T: fprint::TypeFingerprint>(fp: &str) -> bool {
        if T::fingerprint() != fp {
            eprintln!(
                "        success &= assert_type_fprint::<{}>(\"{}\");",
                std::any::type_name::<T>(),
                T::fingerprint()
            );
            false
        } else {
            true
        }
    }

    #[test]
    fn type_fprint_latest_version() {
        eprintln!("latest_version fingerprints:");
        // These should only ever change when adding a new version.
        // The checks below are to ensure that we don't inadvertently change a serialized type.
        // Every versioned_type above should have a corresponding line entry here.
        let mut success = true;
        success &= assert_type_fprint::<AllocatorInfo>("struct {layers:Vec<u64>,allocated_bytes:BTreeMap<u64,u64>,marked_for_deletion:HashSet<u64>,limit_bytes:BTreeMap<u64,u64>}");
        success &= assert_type_fprint::<AllocatorKey>("struct {device_range:Range<u64>}");
        success &=
            assert_type_fprint::<AllocatorValue>("enum {None,Abs(count:u64,owner_object_id:u64)}");
        success &= assert_type_fprint::<EncryptedMutations>("struct {transactions:Vec<(struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},u64,)>,data:Vec<u8>,mutations_key_roll:Vec<(usize,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>}");
        success &= assert_type_fprint::<JournalRecord>("enum {EndBlock,Mutation(object_id:u64,mutation:enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}),Commit,Discard(u64),DidFlushDevice(u64),DataChecksums(Range<u64>,struct {sums:Vec<u8>},bool)}");
        success &= assert_type_fprint::<FsverityMetadata>(
            "struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>}",
        );
        success &= assert_type_fprint::<Mutation>("enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}");
        success &= assert_type_fprint::<ObjectKey>("struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}}");
        success &= assert_type_fprint::<ObjectValue>("enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})}");
        success &=
            assert_type_fprint::<PersistentLayerHeader>("struct {magic:[u8;8],block_size:u64}");
        success &= assert_type_fprint::<PersistentLayerInfo>(
            "struct {num_items:usize,num_data_blocks:u64,bloom_filter_size_bytes:usize,bloom_filter_seed:u64,bloom_filter_num_nonces:usize}",
        );
        success &= assert_type_fprint::<StoreInfo>("struct {guid:[u8;16],last_object_id:u64,layers:Vec<u64>,root_directory_object_id:u64,graveyard_directory_object_id:u64,object_count:u64,mutations_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,mutations_cipher_offset:u64,encrypted_mutations_object_id:u64,object_id_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,internal_directory_object_id:u64}");
        success &= assert_type_fprint::<SuperBlockHeader>("struct {guid:<[u8;16]>,generation:u64,root_parent_store_object_id:u64,root_parent_graveyard_directory_object_id:u64,root_store_object_id:u64,allocator_object_id:u64,journal_object_id:u64,journal_checkpoint:struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},super_block_journal_file_offset:u64,journal_file_offsets:HashMap<u64,u64>,borrowed_metadata_space:u64,earliest_version:struct {major:u32,minor:u8}}");
        success &= assert_type_fprint::<SuperBlockRecord>("enum {Extent(Range<u64>),ObjectItem(struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64}),End}");
        assert!(success, "One or more versioned types have different type fingerprint.");
    }

    #[test]
    fn type_fprint_v46() {
        let mut success = true;
        success &= assert_type_fprint::<AllocatorInfoV32>("struct {layers:Vec<u64>,allocated_bytes:BTreeMap<u64,u64>,marked_for_deletion:HashSet<u64>,limit_bytes:BTreeMap<u64,u64>}");
        success &= assert_type_fprint::<AllocatorKeyV32>("struct {device_range:Range<u64>}");
        success &= assert_type_fprint::<AllocatorValueV32>(
            "enum {None,Abs(count:u64,owner_object_id:u64)}",
        );
        success &= assert_type_fprint::<EncryptedMutationsV40>("struct {transactions:Vec<(struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},u64,)>,data:Vec<u8>,mutations_key_roll:Vec<(usize,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>}");
        success &= assert_type_fprint::<JournalRecordV46>("enum {EndBlock,Mutation(object_id:u64,mutation:enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}),Commit,Discard(u64),DidFlushDevice(u64),DataChecksums(Range<u64>,struct {sums:Vec<u8>},bool)}");
        success &= assert_type_fprint::<FsverityMetadataV33>(
            "struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>}",
        );
        success &= assert_type_fprint::<MutationV46>("enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}");
        success &= assert_type_fprint::<ObjectKeyV43>("struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}}");
        success &= assert_type_fprint::<ObjectValueV46>("enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})}");
        success &=
            assert_type_fprint::<PersistentLayerHeaderV39>("struct {magic:[u8;8],block_size:u64}");
        success &= assert_type_fprint::<PersistentLayerInfoV39>(
            "struct {num_items:usize,num_data_blocks:u64,bloom_filter_size_bytes:usize,bloom_filter_seed:u64,bloom_filter_num_nonces:usize}",
        );
        success &= assert_type_fprint::<StoreInfoV40>("struct {guid:[u8;16],last_object_id:u64,layers:Vec<u64>,root_directory_object_id:u64,graveyard_directory_object_id:u64,object_count:u64,mutations_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,mutations_cipher_offset:u64,encrypted_mutations_object_id:u64,object_id_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,internal_directory_object_id:u64}");
        success &= assert_type_fprint::<SuperBlockHeaderV32>("struct {guid:<[u8;16]>,generation:u64,root_parent_store_object_id:u64,root_parent_graveyard_directory_object_id:u64,root_store_object_id:u64,allocator_object_id:u64,journal_object_id:u64,journal_checkpoint:struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},super_block_journal_file_offset:u64,journal_file_offsets:HashMap<u64,u64>,borrowed_metadata_space:u64,earliest_version:struct {major:u32,minor:u8}}");
        success &= assert_type_fprint::<SuperBlockRecordV46>("enum {Extent(Range<u64>),ObjectItem(struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>),EncryptedSymlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64}),End}");
        assert!(success, "One or more versioned types have different type fingerprint.");
    }

    #[test]
    fn type_fprint_v43() {
        let mut success = true;
        success &= assert_type_fprint::<AllocatorInfoV32>("struct {layers:Vec<u64>,allocated_bytes:BTreeMap<u64,u64>,marked_for_deletion:HashSet<u64>,limit_bytes:BTreeMap<u64,u64>}");
        success &= assert_type_fprint::<AllocatorKeyV32>("struct {device_range:Range<u64>}");
        success &= assert_type_fprint::<AllocatorValueV32>(
            "enum {None,Abs(count:u64,owner_object_id:u64)}",
        );
        success &= assert_type_fprint::<EncryptedMutationsV40>("struct {transactions:Vec<(struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},u64,)>,data:Vec<u8>,mutations_key_roll:Vec<(usize,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>}");
        success &= assert_type_fprint::<JournalRecordV43>("enum {EndBlock,Mutation(object_id:u64,mutation:enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}),Commit,Discard(u64),DidFlushDevice(u64),DataChecksums(Range<u64>,struct {sums:Vec<u8>},bool)}");
        success &= assert_type_fprint::<FsverityMetadataV33>(
            "struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>}",
        );
        success &= assert_type_fprint::<MutationV43>("enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}");
        success &= assert_type_fprint::<ObjectKeyV43>("struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}}");
        success &= assert_type_fprint::<ObjectValueV41>("enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})}");
        success &=
            assert_type_fprint::<PersistentLayerHeaderV39>("struct {magic:[u8;8],block_size:u64}");
        success &= assert_type_fprint::<PersistentLayerInfoV39>(
            "struct {num_items:usize,num_data_blocks:u64,bloom_filter_size_bytes:usize,bloom_filter_seed:u64,bloom_filter_num_nonces:usize}",
        );
        success &= assert_type_fprint::<StoreInfoV40>("struct {guid:[u8;16],last_object_id:u64,layers:Vec<u64>,root_directory_object_id:u64,graveyard_directory_object_id:u64,object_count:u64,mutations_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,mutations_cipher_offset:u64,encrypted_mutations_object_id:u64,object_id_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,internal_directory_object_id:u64}");
        success &= assert_type_fprint::<SuperBlockHeaderV32>("struct {guid:<[u8;16]>,generation:u64,root_parent_store_object_id:u64,root_parent_graveyard_directory_object_id:u64,root_store_object_id:u64,allocator_object_id:u64,journal_object_id:u64,journal_checkpoint:struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},super_block_journal_file_offset:u64,journal_file_offsets:HashMap<u64,u64>,borrowed_metadata_space:u64,earliest_version:struct {major:u32,minor:u8}}");
        success &= assert_type_fprint::<SuperBlockRecordV43>("enum {Extent(Range<u64>),ObjectItem(struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(casefold_hash:u32,name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64}),End}");
        assert!(success, "One or more versioned types have different type fingerprint.");
    }

    #[test]
    fn type_fprint_v42() {
        let mut success = true;

        success &= assert_type_fprint::<AllocatorInfoV32>("struct {layers:Vec<u64>,allocated_bytes:BTreeMap<u64,u64>,marked_for_deletion:HashSet<u64>,limit_bytes:BTreeMap<u64,u64>}");
        success &= assert_type_fprint::<AllocatorKeyV32>("struct {device_range:Range<u64>}");
        success &= assert_type_fprint::<AllocatorValueV32>(
            "enum {None,Abs(count:u64,owner_object_id:u64)}",
        );
        success &= assert_type_fprint::<EncryptedMutationsV40>("struct {transactions:Vec<(struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},u64,)>,data:Vec<u8>,mutations_key_roll:Vec<(usize,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>}");
        success &= assert_type_fprint::<JournalRecordV42>("enum {EndBlock,Mutation(object_id:u64,mutation:enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}),Commit,Discard(u64),DidFlushDevice(u64),DataChecksums(Range<u64>,struct {sums:Vec<u8>},bool)}");
        success &= assert_type_fprint::<FsverityMetadataV33>(
            "struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>}",
        );
        success &= assert_type_fprint::<MutationV41>("enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}");
        success &= assert_type_fprint::<ObjectKeyV40>("struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}}");
        success &= assert_type_fprint::<ObjectValueV41>("enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})}");
        success &=
            assert_type_fprint::<PersistentLayerHeaderV39>("struct {magic:[u8;8],block_size:u64}");
        success &= assert_type_fprint::<PersistentLayerInfoV39>(
            "struct {num_items:usize,num_data_blocks:u64,bloom_filter_size_bytes:usize,bloom_filter_seed:u64,bloom_filter_num_nonces:usize}",
        );
        success &= assert_type_fprint::<StoreInfoV40>("struct {guid:[u8;16],last_object_id:u64,layers:Vec<u64>,root_directory_object_id:u64,graveyard_directory_object_id:u64,object_count:u64,mutations_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,mutations_cipher_offset:u64,encrypted_mutations_object_id:u64,object_id_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,internal_directory_object_id:u64}");
        success &= assert_type_fprint::<SuperBlockHeaderV32>("struct {guid:<[u8;16]>,generation:u64,root_parent_store_object_id:u64,root_parent_graveyard_directory_object_id:u64,root_store_object_id:u64,allocator_object_id:u64,journal_object_id:u64,journal_checkpoint:struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},super_block_journal_file_offset:u64,journal_file_offsets:HashMap<u64,u64>,borrowed_metadata_space:u64,earliest_version:struct {major:u32,minor:u8}}");
        success &= assert_type_fprint::<SuperBlockRecordV41>("enum {Extent(Range<u64>),ObjectItem(struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64}),End}");
        assert!(success, "One or more versioned types have different type fingerprint.");
    }

    #[test]
    fn type_fprint_v41() {
        let mut success = true;

        success &= assert_type_fprint::<AllocatorInfoV32>("struct {layers:Vec<u64>,allocated_bytes:BTreeMap<u64,u64>,marked_for_deletion:HashSet<u64>,limit_bytes:BTreeMap<u64,u64>}");
        success &= assert_type_fprint::<AllocatorKeyV32>("struct {device_range:Range<u64>}");
        success &= assert_type_fprint::<AllocatorValueV32>(
            "enum {None,Abs(count:u64,owner_object_id:u64)}",
        );
        success &= assert_type_fprint::<EncryptedMutationsV40>("struct {transactions:Vec<(struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},u64,)>,data:Vec<u8>,mutations_key_roll:Vec<(usize,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>}");
        success &= assert_type_fprint::<JournalRecordV41>("enum {EndBlock,Mutation(object_id:u64,mutation:enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}),Commit,Discard(u64),DidFlushDevice(u64),DataChecksums(Range<u64>,struct {sums:Vec<u8>})}");
        success &= assert_type_fprint::<FsverityMetadataV33>(
            "struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>}",
        );
        success &= assert_type_fprint::<MutationV41>("enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}");
        success &= assert_type_fprint::<ObjectKeyV40>("struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}}");
        success &= assert_type_fprint::<ObjectValueV41>("enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})}");
        success &=
            assert_type_fprint::<PersistentLayerHeaderV39>("struct {magic:[u8;8],block_size:u64}");
        success &= assert_type_fprint::<PersistentLayerInfoV39>(
            "struct {num_items:usize,num_data_blocks:u64,bloom_filter_size_bytes:usize,bloom_filter_seed:u64,bloom_filter_num_nonces:usize}",
        );
        success &= assert_type_fprint::<StoreInfoV40>("struct {guid:[u8;16],last_object_id:u64,layers:Vec<u64>,root_directory_object_id:u64,graveyard_directory_object_id:u64,object_count:u64,mutations_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,mutations_cipher_offset:u64,encrypted_mutations_object_id:u64,object_id_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,internal_directory_object_id:u64}");
        success &= assert_type_fprint::<SuperBlockHeaderV32>("struct {guid:<[u8;16]>,generation:u64,root_parent_store_object_id:u64,root_parent_graveyard_directory_object_id:u64,root_store_object_id:u64,allocator_object_id:u64,journal_object_id:u64,journal_checkpoint:struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},super_block_journal_file_offset:u64,journal_file_offsets:HashMap<u64,u64>,borrowed_metadata_space:u64,earliest_version:struct {major:u32,minor:u8}}");
        success &= assert_type_fprint::<SuperBlockRecordV41>("enum {Extent(Range<u64>),ObjectItem(struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64,has_overwrite_extents:bool),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64}),End}");
        assert!(success, "One or more versioned types have different type fingerprint.");
    }

    #[test]
    fn type_fprint_v40() {
        let mut success = true;
        success &= assert_type_fprint::<AllocatorInfoV32>("struct {layers:Vec<u64>,allocated_bytes:BTreeMap<u64,u64>,marked_for_deletion:HashSet<u64>,limit_bytes:BTreeMap<u64,u64>}");
        success &= assert_type_fprint::<AllocatorKeyV32>("struct {device_range:Range<u64>}");
        success &= assert_type_fprint::<AllocatorValueV32>(
            "enum {None,Abs(count:u64,owner_object_id:u64)}",
        );
        success &= assert_type_fprint::<EncryptedMutationsV40>("struct {transactions:Vec<(struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},u64,)>,data:Vec<u8>,mutations_key_roll:Vec<(usize,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>}");
        success &= assert_type_fprint::<JournalRecordV40>("enum {EndBlock,Mutation(object_id:u64,mutation:enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64,has_overwrite_extents:bool),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}),Commit,Discard(u64),DidFlushDevice(u64),DataChecksums(Range<u64>,struct {sums:Vec<u8>})}");
        success &= assert_type_fprint::<FsverityMetadataV33>(
            "struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>}",
        );
        success &= assert_type_fprint::<MutationV40>("enum {ObjectStore(struct {item:struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64,has_overwrite_extents:bool),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64},op:enum {Insert,ReplaceOrInsert,Merge}}),EncryptedObjectStore(Box<[u8]>),Allocator(enum {Allocate(device_range:struct {Range<u64>},owner_object_id:u64),Deallocate(device_range:struct {Range<u64>},owner_object_id:u64),SetLimit(owner_object_id:u64,bytes:u64),MarkForDeletion(u64)}),BeginFlush,EndFlush,DeleteVolume,UpdateBorrowed(u64),UpdateMutationsKey(struct {struct {wrapping_key_id:u128,key:WrappedKeyBytes}}),CreateInternalDir(u64)}");
        success &= assert_type_fprint::<ObjectKeyV40>("struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}}");
        success &= assert_type_fprint::<ObjectValueV40>("enum {None,Some,Object(kind:enum {File(refs:u64,has_overwrite_extents:bool),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})}");
        success &=
            assert_type_fprint::<PersistentLayerHeaderV39>("struct {magic:[u8;8],block_size:u64}");
        success &= assert_type_fprint::<PersistentLayerInfoV39>(
            "struct {num_items:usize,num_data_blocks:u64,bloom_filter_size_bytes:usize,bloom_filter_seed:u64,bloom_filter_num_nonces:usize}",
        );
        success &= assert_type_fprint::<StoreInfoV40>("struct {guid:[u8;16],last_object_id:u64,layers:Vec<u64>,root_directory_object_id:u64,graveyard_directory_object_id:u64,object_count:u64,mutations_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,mutations_cipher_offset:u64,encrypted_mutations_object_id:u64,object_id_key:Option<struct {wrapping_key_id:u128,key:WrappedKeyBytes}>,internal_directory_object_id:u64}");
        success &= assert_type_fprint::<SuperBlockHeaderV32>("struct {guid:<[u8;16]>,generation:u64,root_parent_store_object_id:u64,root_parent_graveyard_directory_object_id:u64,root_store_object_id:u64,allocator_object_id:u64,journal_object_id:u64,journal_checkpoint:struct {file_offset:u64,checksum:u64,version:struct {major:u32,minor:u8}},super_block_journal_file_offset:u64,journal_file_offsets:HashMap<u64,u64>,borrowed_metadata_space:u64,earliest_version:struct {major:u32,minor:u8}}");
        success &= assert_type_fprint::<SuperBlockRecordV40>("enum {Extent(Range<u64>),ObjectItem(struct {key:struct {object_id:u64,data:enum {Object,Keys,Attribute(u64,enum {Attribute,Extent(struct {range:Range<u64>})}),Child(name:String),GraveyardEntry(object_id:u64),Project(project_id:u64,property:enum {Limit,Usage}),ExtendedAttribute(name:Vec<u8>),GraveyardAttributeEntry(object_id:u64,attribute_id:u64),EncryptedChild(name:Vec<u8>),CasefoldChild(name:struct {String})}},value:enum {None,Some,Object(kind:enum {File(refs:u64,has_overwrite_extents:bool),Directory(sub_dirs:u64,wrapping_key_id:Option<u128>,casefold:bool),Graveyard,Symlink(refs:u64,link:Vec<u8>)},attributes:struct {creation_time:struct {secs:u64,nanos:u32},modification_time:struct {secs:u64,nanos:u32},project_id:u64,posix_attributes:Option<struct {mode:u32,uid:u32,gid:u32,rdev:u64}>,allocated_size:u64,access_time:struct {secs:u64,nanos:u32},change_time:struct {secs:u64,nanos:u32}}),Keys(enum {AES256XTS(struct {Vec<(u64,struct {wrapping_key_id:u128,key:WrappedKeyBytes},)>})}),Attribute(size:u64),Extent(enum {None,Some(device_offset:u64,mode:enum {Raw,Cow(struct {sums:Vec<u8>}),OverwritePartial(bit_vec :: BitVec<u32>),Overwrite},key_id:u64)}),Child(struct {object_id:u64,object_descriptor:enum {File,Directory,Volume,Symlink}}),Trim,BytesAndNodes(bytes:i64,nodes:i64),ExtendedAttribute(enum {Inline(Vec<u8>),AttributeId(u64)}),VerifiedAttribute(size:u64,fsverity_metadata:struct {root_digest:enum {Sha256([u8;32]),Sha512(Vec<u8>)},salt:Vec<u8>})},sequence:u64}),End}");
        assert!(success, "One or more versioned types have different type fingerprint.");
    }
}
