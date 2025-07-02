// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::{
    emit_dotdot, fileops_impl_directory, fileops_impl_noop_sync, fileops_impl_unbounded_seek,
    fs_node_impl_dir_readonly, DirectoryEntryType, DirentSink, FileObject, FileOps, FileSystem,
    FileSystemHandle, FsNode, FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr,
};
use starnix_sync::{FileOpsCore, Locked};
use starnix_uapi::auth::FsCred;
use starnix_uapi::device_type::DeviceType;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{mode, FileMode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, gid_t, uid_t};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Builds an implementation of [`FsNodeOps`] that serves as a directory of static and immutable
/// entries.
pub struct StaticDirectoryBuilder<'a> {
    fs: &'a Arc<FileSystem>,
    mode: FileMode,
    creds: FsCred,
    entries: BTreeMap<&'static FsStr, FsNodeHandle>,
}

impl<'a> StaticDirectoryBuilder<'a> {
    /// Creates a new builder using the given [`FileSystem`] to acquire inode numbers.
    pub fn new(fs: &'a FileSystemHandle) -> Self {
        Self { fs, mode: mode!(IFDIR, 0o777), creds: FsCred::root(), entries: BTreeMap::new() }
    }

    /// Adds an entry to the directory. Panics if an entry with the same name was already added.
    pub fn entry(
        &mut self,
        name: &'static str,
        ops: impl Into<Box<dyn FsNodeOps>>,
        mode: FileMode,
    ) {
        self.entry_etc(name, ops, mode, DeviceType::NONE, FsCred::root());
    }

    /// Adds an entry to the directory. Panics if an entry with the same name was already added.
    pub fn entry_etc(
        &mut self,
        name: &'static str,
        ops: impl Into<Box<dyn FsNodeOps>>,
        mode: FileMode,
        dev: DeviceType,
        creds: FsCred,
    ) {
        let ops = ops.into();
        let mut info = FsNodeInfo::new(mode, creds);
        info.rdev = dev;
        let node = self.fs.create_node_and_allocate_node_id(ops, info);
        self.node(name, node);
    }

    pub fn subdir(&mut self, name: &'static str, mode: u32, build_subdir: impl Fn(&mut Self)) {
        let mut subdir = Self::new(self.fs);
        build_subdir(&mut subdir);
        subdir.set_mode(mode!(IFDIR, mode));
        self.node(name, subdir.build());
    }

    /// Adds an [`FsNode`] entry to the directory, which already has an inode number and file mode.
    /// Panics if an entry with the same name was already added.
    pub fn node(&mut self, name: &'static str, node: FsNodeHandle) {
        assert!(
            self.entries.insert(name.into(), node).is_none(),
            "adding a duplicate entry into a StaticDirectory",
        );
    }

    /// Set the mode of the directory. The type must always be IFDIR.
    pub fn set_mode(&mut self, mode: FileMode) {
        assert!(mode.is_dir());
        self.mode = mode;
    }

    pub fn dir_creds(&mut self, creds: FsCred) {
        self.creds = creds;
    }

    /// Builds an [`FsNode`] that serves as a directory of the entries added to this builder.
    pub fn build(self) -> FsNodeHandle {
        self.fs.create_node_and_allocate_node_id(
            Arc::new(StaticDirectory { entries: self.entries }),
            FsNodeInfo::new(self.mode, self.creds),
        )
    }

    /// Build the node associated with the static directory and makes it the root of the
    /// filesystem.
    pub fn build_root(self) {
        let ops = Arc::new(StaticDirectory { entries: self.entries });
        let root_ino = self.fs.allocate_ino();
        self.fs.create_root_with_info(root_ino, ops, FsNodeInfo::new(self.mode, self.creds));
    }

    /// Builds [`FsNodeOps`] for this directory.
    pub fn build_ops(self) -> Box<dyn FsNodeOps> {
        let directory = Arc::new(StaticDirectory { entries: self.entries });
        Box::new(directory)
    }
}

pub struct StaticDirectory {
    entries: BTreeMap<&'static FsStr, FsNodeHandle>,
}

impl StaticDirectory {
    pub fn force_chown(&self, current_task: &CurrentTask, uid: Option<uid_t>, gid: Option<gid_t>) {
        for (_, node) in self.entries.iter() {
            node.update_info(|info| {
                info.chown(uid, gid);
            });

            let Some(static_dir) = node.downcast_ops::<Arc<StaticDirectory>>() else {
                continue;
            };
            static_dir.force_chown(current_task, uid, gid);
        }
    }
}

impl FsNodeOps for Arc<StaticDirectory> {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        self.entries.get(name).cloned().ok_or_else(|| {
            errno!(
                ENOENT,
                format!(
                    "looking for {name} in {:?}",
                    self.entries.keys().map(|e| e.to_string()).collect::<Vec<_>>()
                )
            )
        })
    }
}

impl FileOps for StaticDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        emit_dotdot(file, sink)?;

        // Skip through the entries until the current offset is reached.
        // Subtract 2 from the offset to account for `.` and `..`.
        for (name, node) in self.entries.iter().skip(sink.offset() as usize - 2) {
            sink.add(
                node.ino,
                sink.offset() + 1,
                DirectoryEntryType::from_mode(node.info().mode),
                name,
            )?;
        }
        Ok(())
    }
}
