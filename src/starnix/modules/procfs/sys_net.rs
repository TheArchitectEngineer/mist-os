// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::task::CurrentTask;
use starnix_core::vfs::stub_bytes_file::StubBytesFile;
use starnix_core::vfs::{
    emit_dotdot, fileops_impl_directory, fileops_impl_noop_sync, fs_node_impl_dir_readonly,
    unbounded_seek, DirectoryEntryType, DirentSink, FileObject, FileOps, FsNode, FsNodeHandle,
    FsNodeOps, FsStr, SeekTarget, StaticDirectoryBuilder,
};
use starnix_logging::bug_ref;
use starnix_sync::{FileOpsCore, Locked};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{mode, FileMode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{error, off_t};

const FILE_MODE: FileMode = mode!(IFREG, 0o644);

fn netstack_devices_readdir(
    _locked: &mut Locked<'_, FileOpsCore>,
    file: &FileObject,
    current_task: &CurrentTask,
    sink: &mut dyn DirentSink,
) -> Result<(), Errno> {
    emit_dotdot(file, sink)?;

    if sink.offset() == 2 {
        sink.add(
            file.fs.next_node_id(),
            sink.offset() + 1,
            DirectoryEntryType::from_mode(FILE_MODE),
            "all".into(),
        )?;
    }

    if sink.offset() == 3 {
        sink.add(
            file.fs.next_node_id(),
            sink.offset() + 1,
            DirectoryEntryType::from_mode(FILE_MODE),
            "default".into(),
        )?;
    }

    let devices = current_task.kernel().netstack_devices.snapshot_devices();
    for (name, _) in devices.iter().skip(sink.offset() as usize - 4) {
        let inode_num = file.fs.next_node_id();
        sink.add(
            inode_num,
            sink.offset() + 1,
            DirectoryEntryType::from_mode(FILE_MODE),
            name.as_ref(),
        )?;
    }
    Ok(())
}

fn has_netstack_device(current_task: &CurrentTask, name: &FsStr) -> bool {
    // Per https://www.kernel.org/doc/Documentation/networking/ip-sysctl.txt,
    //
    //   conf/default/*:
    //	   Change the interface-specific default settings.
    //
    //   conf/all/*:
    //	   Change all the interface-specific settings.
    //
    // Note that the all/default directories don't exist in `/sys/class/net`.
    name == "all"
        || name == "default"
        || current_task.kernel().netstack_devices.get_device(name).is_some()
}

#[derive(Clone)]
pub struct ProcSysNetIpv4Conf;

impl FsNodeOps for ProcSysNetIpv4Conf {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if has_netstack_device(current_task, name) {
            let fs = node.fs();
            let mut dir = StaticDirectoryBuilder::new(&fs);
            dir.entry(
                current_task,
                "accept_redirects",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv4/DEVICE/conf/accept_redirects",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            return Ok(dir.build(current_task));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv4Conf {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();

    fn seek(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        unbounded_seek(current_offset, target)
    }

    fn readdir(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        netstack_devices_readdir(locked, file, current_task, sink)
    }
}

#[derive(Clone)]
pub struct ProcSysNetIpv4Neigh;

impl FsNodeOps for ProcSysNetIpv4Neigh {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if has_netstack_device(current_task, name) {
            let fs = node.fs();
            let mut dir = StaticDirectoryBuilder::new(&fs);
            dir.entry(
                current_task,
                "ucast_solicit",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv4/DEVICE/neigh/ucast_solicit",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "retrans_time_ms",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv4/DEVICE/neigh/retrans_time_ms",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "mcast_resolicit",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv4/DEVICE/neigh/mcast_resolicit",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "base_reachable_time_ms",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv4/DEVICE/neigh/base_reachable_time_ms",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            return Ok(dir.build(current_task));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv4Neigh {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();

    fn seek(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        unbounded_seek(current_offset, target)
    }

    fn readdir(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        netstack_devices_readdir(locked, file, current_task, sink)
    }
}

#[derive(Clone)]
pub struct ProcSysNetIpv6Conf;

impl FsNodeOps for ProcSysNetIpv6Conf {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if has_netstack_device(current_task, name) {
            let fs = node.fs();
            let mut dir = StaticDirectoryBuilder::new(&fs);
            dir.entry(
                current_task,
                "accept_ra",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/accept_ra",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "accept_ra_defrtr",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/accept_ra_defrtr",
                    bug_ref!("https://fxbug.dev/322907588"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "accept_ra_info_min_plen",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/accept_ra_info_min_plen",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "accept_ra_rt_info_min_plen",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/accept_ra_rt_info_min_plen",
                    bug_ref!("https://fxbug.dev/322908046"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "accept_ra_rt_table",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/accept_ra_rt_table",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "accept_redirects",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/accept_redirects",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "dad_transmits",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/dad_transmits",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "use_tempaddr",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/use_tempaddr",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "addr_gen_mode",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/addr_gen_mode",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "stable_secret",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/stable_secret",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "disable_ipv6",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/disable_ip64",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "optimistic_dad",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/optimistic_dad",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "use_oif_addrs_only",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/use_oif_addrs_only",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "use_optimistic",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/use_optimistic",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "forwarding",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/conf/forwarding",
                    bug_ref!("https://fxbug.dev/322907925"),
                ),
                FILE_MODE,
            );
            return Ok(dir.build(current_task));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv6Conf {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();

    fn seek(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        unbounded_seek(current_offset, target)
    }

    fn readdir(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        netstack_devices_readdir(locked, file, current_task, sink)
    }
}

#[derive(Clone)]
pub struct ProcSysNetIpv6Neigh;

impl FsNodeOps for ProcSysNetIpv6Neigh {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if has_netstack_device(current_task, name) {
            let fs = node.fs();
            let mut dir = StaticDirectoryBuilder::new(&fs);
            dir.entry(
                current_task,
                "ucast_solicit",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/neigh/ucast_solicit",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "retrans_time_ms",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/neigh/retrans_time_ms",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "mcast_resolicit",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/neigh/mcast_resolicit",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            dir.entry(
                current_task,
                "base_reachable_time_ms",
                StubBytesFile::new_node(
                    "/proc/sys/net/ipv6/DEVICE/neigh/base_reachable_time_ms",
                    bug_ref!("https://fxbug.dev/297439563"),
                ),
                FILE_MODE,
            );
            return Ok(dir.build(current_task));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv6Neigh {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();

    fn seek(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        unbounded_seek(current_offset, target)
    }

    fn readdir(
        &self,
        locked: &mut Locked<'_, FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        netstack_devices_readdir(locked, file, current_task, sink)
    }
}
