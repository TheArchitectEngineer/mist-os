// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::syscalls::do_clone;
use crate::task::CurrentTask;
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::{clone_args, tid_t, CSIGNAL};

use starnix_sync::{Locked, Unlocked};

/// The parameter order for `clone` varies by architecture.
pub fn sys_clone(
    locked: &mut Locked<Unlocked>,
    current_task: &mut CurrentTask,
    flags: u64,
    user_stack: UserAddress,
    user_parent_tid: UserRef<tid_t>,
    user_tls: UserAddress,
    user_child_tid: UserRef<tid_t>,
) -> Result<tid_t, Errno> {
    // Our flags parameter uses the low 8 bits (CSIGNAL mask) of flags to indicate the exit
    // signal. The CloneArgs struct separates these as `flags` and `exit_signal`.
    do_clone(
        locked,
        current_task,
        &clone_args {
            flags: flags & !(CSIGNAL as u64),
            child_tid: user_child_tid.addr().ptr() as u64,
            parent_tid: user_parent_tid.addr().ptr() as u64,
            exit_signal: flags & (CSIGNAL as u64),
            stack: user_stack.ptr() as u64,
            tls: user_tls.ptr() as u64,
            ..Default::default()
        },
    )
}
