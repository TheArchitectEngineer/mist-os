// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::kobject::KObjectHandle;
use crate::fs::sysfs::{
    sysfs_kernel_directory, sysfs_power_directory, CpuClassDirectory, KObjectDirectory,
    VulnerabilitiesClassDirectory,
};
use crate::task::CurrentTask;
use crate::vfs::pseudo::simple_directory::{SimpleDirectory, SimpleDirectoryMutator};
use crate::vfs::pseudo::simple_file::BytesFile;
use crate::vfs::pseudo::stub_empty_file::StubEmptyFile;
use crate::vfs::{
    CacheConfig, CacheMode, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions,
    FsNodeInfo, FsStr, PathBuilder, SymlinkNode,
};
use ebpf_api::BPF_PROG_TYPE_FUSE;
use starnix_logging::bug_ref;
use starnix_sync::{FileOpsCore, Locked, Unlocked};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::FsCred;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::{statfs, SYSFS_MAGIC};

pub const SYSFS_DEVICES: &str = "devices";
pub const SYSFS_BUS: &str = "bus";
pub const SYSFS_CLASS: &str = "class";
pub const SYSFS_BLOCK: &str = "block";
pub const SYSFS_DEV: &str = "dev";

struct SysFs;
impl FileSystemOps for SysFs {
    fn statfs(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _fs: &FileSystem,
        _current_task: &CurrentTask,
    ) -> Result<statfs, Errno> {
        Ok(default_statfs(SYSFS_MAGIC))
    }
    fn name(&self) -> &'static FsStr {
        "sysfs".into()
    }
}

impl SysFs {
    fn new_fs(current_task: &CurrentTask, options: FileSystemOptions) -> FileSystemHandle {
        let kernel = current_task.kernel();
        let registry = &kernel.device_registry;

        let fs = FileSystem::new_with_node_cache(
            kernel,
            CacheMode::Cached(CacheConfig::default()),
            SysFs,
            options,
            registry.objects.node_cache.clone(),
        )
        .expect("sysfs constructed with valid options");

        let root = SimpleDirectory::new();
        fs.create_root(fs.allocate_ino(), root.clone());
        let dir = SimpleDirectoryMutator::new(fs.clone(), root);

        let dir_mode = mode!(IFDIR, 0o755);
        dir.subdir("fs", 0o755, |dir| {
            dir.subdir("selinux", 0o755, |_| ());
            dir.subdir("bpf", 0o755, |_| ());
            dir.subdir("cgroup", 0o755, |_| ());
            dir.subdir("fuse", 0o755, |dir| {
                dir.subdir("connections", 0o755, |_| ());
                dir.subdir("features", 0o755, |dir| {
                    dir.entry(
                        "fuse_bpf",
                        BytesFile::new_node(b"supported\n".to_vec()),
                        mode!(IFREG, 0o444),
                    );
                });
                dir.entry(
                    "bpf_prog_type_fuse",
                    BytesFile::new_node(format!("{}\n", BPF_PROG_TYPE_FUSE).into_bytes()),
                    mode!(IFREG, 0o444),
                );
            });
            dir.subdir("pstore", 0o755, |_| ());
        });

        dir.entry(SYSFS_DEVICES, registry.objects.devices.ops(), dir_mode);
        dir.entry(SYSFS_BUS, registry.objects.bus.ops(), dir_mode);
        dir.entry(SYSFS_BLOCK, registry.objects.block.ops(), dir_mode);
        dir.entry(SYSFS_CLASS, registry.objects.class.ops(), dir_mode);
        dir.entry(SYSFS_DEV, registry.objects.dev.ops(), dir_mode);

        sysfs_kernel_directory(current_task, &dir);
        sysfs_power_directory(current_task, &dir);

        dir.subdir("module", 0o755, |dir| {
            dir.subdir("dm_verity", 0o755, |dir| {
                dir.subdir("parameters", 0o755, |dir| {
                    dir.entry(
                        "prefetch_cluster",
                        StubEmptyFile::new_node(
                            "/sys/module/dm_verity/paramters/prefetch_cluster",
                            bug_ref!("https://fxbug.dev/322893670"),
                        ),
                        mode!(IFREG, 0o644),
                    );
                });
            });
        });

        // TODO(https://fxbug.dev/42072346): Temporary fix of flakeness in tcp_socket_test.
        // Remove after registry.rs refactor is in place.
        registry
            .objects
            .devices
            .get_or_create_child("system".into(), KObjectDirectory::new)
            .get_or_create_child("cpu".into(), CpuClassDirectory::new)
            .get_or_create_child("vulnerabilities".into(), VulnerabilitiesClassDirectory::new);

        fs
    }
}

struct SysFsHandle(FileSystemHandle);

pub fn sys_fs(
    _locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    Ok(current_task
        .kernel()
        .expando
        .get_or_init(|| SysFsHandle(SysFs::new_fs(current_task, options)))
        .0
        .clone())
}

/// Creates a path to the `to` kobject in the devices tree, relative to the `from` kobject from
/// a subsystem.
pub fn sysfs_create_link(
    from: KObjectHandle,
    to: KObjectHandle,
    owner: FsCred,
) -> (SymlinkNode, FsNodeInfo) {
    let mut path = PathBuilder::new();
    path.prepend_element(to.path().as_ref());
    // Escape one more level from its subsystem to the root of sysfs.
    path.prepend_element("..".into());

    let path_to_root = from.path_to_root();
    if !path_to_root.is_empty() {
        path.prepend_element(path_to_root.as_ref());
    }

    // Build a symlink with the relative path.
    SymlinkNode::new(path.build_relative().as_ref(), owner)
}

/// Creates a path to the `to` kobject in the devices tree, relative to the `from` kobject from
/// the bus devices directory.
pub fn sysfs_create_bus_link(
    from: KObjectHandle,
    to: KObjectHandle,
    owner: FsCred,
) -> (SymlinkNode, FsNodeInfo) {
    let mut path = PathBuilder::new();
    path.prepend_element(to.path().as_ref());
    // Escape two more levels from its subsystem to the root of sysfs.
    path.prepend_element("..".into());
    path.prepend_element("..".into());

    let path_to_root = from.path_to_root();
    if !path_to_root.is_empty() {
        path.prepend_element(path_to_root.as_ref());
    }

    // Build a symlink with the relative path.
    SymlinkNode::new(path.build_relative().as_ref(), owner)
}
