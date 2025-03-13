// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::{fs_node_impl_symlink, FsNode, FsNodeOps, SymlinkTarget};
use starnix_sync::{FileOpsCore, Locked};
use starnix_uapi::errors::Errno;

/// A node that represents a symlink to `proc/<pid>` where <pid> is the pid of the task that
/// reads the `proc/self` symlink.
pub struct SelfSymlink;

impl SelfSymlink {
    pub fn new_node() -> impl FsNodeOps {
        Self {}
    }
}

impl FsNodeOps for SelfSymlink {
    fs_node_impl_symlink!();

    fn readlink(
        &self,
        _locked: &mut Locked<'_, FileOpsCore>,
        _node: &FsNode,
        current_task: &CurrentTask,
    ) -> Result<SymlinkTarget, Errno> {
        Ok(SymlinkTarget::Path(current_task.get_pid().to_string().into()))
    }
}
