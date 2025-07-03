// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::{
    default_ioctl, fileops_impl_nonseekable, fileops_impl_noop_sync, Anon, FileHandle, FileObject,
    FileOps,
};
#[cfg(not(feature = "starnix_lite"))]
use starnix_logging::log_info;
use starnix_sync::{FileOpsCore, LockEqualOrBefore, Locked, Unlocked};
use starnix_syscalls::{SyscallArg, SyscallResult};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;

pub struct SyslogFile;

impl SyslogFile {
    pub fn new_file<L>(locked: &mut Locked<L>, current_task: &CurrentTask) -> FileHandle
    where
        L: LockEqualOrBefore<FileOpsCore>,
    {
        // TODO: https://fxbug.dev/404739824 - Use a non-private node once labeling of external resources is addressed.
        Anon::new_private_file(
            locked,
            current_task,
            Box::new(SyslogFile),
            OpenFlags::RDWR,
            "[fuchsia:syslog]",
        )
    }
}

impl FileOps for SyslogFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        data.read_each(&mut |bytes| {
            #[cfg(not(feature = "starnix_lite"))]
            log_info!(tag = "stdio"; "{}", String::from_utf8_lossy(bytes));
            #[cfg(feature = "starnix_lite")]
            print!("{}", String::from_utf8_lossy(bytes));
            Ok(bytes.len())
        })
    }

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        Ok(0)
    }

    fn ioctl(
        &self,
        locked: &mut Locked<Unlocked>,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        default_ioctl(file, locked, current_task, request, arg)
    }
}
