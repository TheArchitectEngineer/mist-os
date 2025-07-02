// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::kobject::KObject;
use crate::fs::tmpfs::TmpfsDirectory;
use crate::task::CurrentTask;
use crate::vfs::pseudo::simple_file::BytesFile;
use crate::vfs::pseudo::vec_directory::{VecDirectory, VecDirectoryEntry};
use crate::vfs::{
    fs_node_impl_dir_readonly, DirectoryEntryType, FileOps, FsNode, FsNodeHandle, FsNodeInfo,
    FsNodeOps, FsStr,
};

use starnix_sync::{FileOpsCore, Locked};
use starnix_uapi::auth::FsCred;
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use std::collections::HashMap;
use std::sync::Weak;

// Matches file names and creates corresponding files with specified content.
macro_rules! file_match_and_create {
    ($node:expr, $name:expr, $files:expr) => {
        if let Some(content) = $files.get(&std::str::from_utf8(&**$name).unwrap()) {
            Ok($node.fs().create_node_and_allocate_node_id(
                BytesFile::new_node(content.as_bytes().to_vec()),
                FsNodeInfo::new(mode!(IFREG, 0o444), FsCred::root()),
            ))
        } else {
            error!(ENOENT)
        }
    };
}

pub struct VulnerabilitiesClassDirectory {
    vulnerability_files: HashMap<&'static str, &'static str>,
}

impl VulnerabilitiesClassDirectory {
    pub fn new(_kobject: Weak<KObject>) -> Self {
        // TODO(b/395160526): Dynamically generate these files based on CPU type.
        let mut files = HashMap::new();
        files.insert("gather_data_sampling", "Not affected\n");
        files.insert("itlb_multihit", "Not affected\n");
        files.insert("l1tf", "Not affected\n");
        files.insert("mds", "Not affected\n");
        files.insert("meltdown", "Not affected\n");
        files.insert("mmio_stale_data", "Not affected\n");
        files.insert("retbleed", "Not affected\n");
        files.insert("spec_rstack_overflow", "Not affected\n");
        files.insert("spec_store_bypass", "Not affected\n");
        files.insert("spectre_v1", "Not affected\n");
        files.insert("spectre_v2", "Not affected\n");
        files.insert("srbds", "Not affected\n");
        files.insert("tsx_async_abort", "Not affected\n");
        Self { vulnerability_files: files }
    }
}

impl FsNodeOps for VulnerabilitiesClassDirectory {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let entries = self
            .vulnerability_files
            .keys()
            .map(|name| VecDirectoryEntry {
                entry_type: DirectoryEntryType::REG,
                name: (*name).into(),
                inode: None,
            })
            .collect::<Vec<_>>();

        Ok(VecDirectory::new_file(entries))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if name.starts_with(b"vulnerabilities") {
            Ok(node.fs().create_node_and_allocate_node_id(
                TmpfsDirectory::new(),
                FsNodeInfo::new(mode!(IFDIR, 0o755), FsCred::root()),
            ))
        } else {
            file_match_and_create!(node, name, self.vulnerability_files)
        }
    }
}
