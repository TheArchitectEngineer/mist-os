// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://github.com/rust-lang/rust/issues/39371): remove
#![allow(non_upper_case_globals)]

use super::{
    check_permission, check_self_permission, task_effective_sid, BpfMapState, BpfProgState,
};

use crate::bpf::program::Program;
use crate::bpf::BpfMap;
use crate::security::PermissionFlags;
use crate::task::CurrentTask;
use selinux::{BpfPermission, SecurityId, SecurityServer};
use starnix_uapi::errors::Errno;
use starnix_uapi::{bpf_cmd, bpf_cmd_BPF_MAP_CREATE, bpf_cmd_BPF_PROG_LOAD, bpf_cmd_BPF_PROG_RUN};
use zerocopy::FromBytes;

/// Returns the security state to be assigned to a BPF map. This is defined as the security
/// context of the creating task.
pub(in crate::security) fn bpf_map_alloc(current_task: &CurrentTask) -> BpfMapState {
    BpfMapState { sid: task_effective_sid(current_task) }
}

/// Returns the security state to be assigned to a BPF program. This is defined as the
/// security context of the creating task.
pub(in crate::security) fn bpf_prog_alloc(current_task: &CurrentTask) -> BpfProgState {
    BpfProgState { sid: task_effective_sid(current_task) }
}

/// Returns whether `current_task` can perform the bpf `cmd`.
pub(in crate::security) fn check_bpf_access<Attr: FromBytes>(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    cmd: bpf_cmd,
    _attr: &Attr,
    _attr_size: u32,
) -> Result<(), Errno> {
    let audit_context = current_task.into();

    let sid: SecurityId = task_effective_sid(current_task);
    let permission = match cmd {
        bpf_cmd_BPF_MAP_CREATE => BpfPermission::MapCreate,
        bpf_cmd_BPF_PROG_LOAD => BpfPermission::ProgLoad,
        bpf_cmd_BPF_PROG_RUN => BpfPermission::ProgRun,
        _ => return Ok(()),
    };
    check_self_permission(
        &security_server.as_permission_check(),
        current_task.kernel(),
        sid,
        permission,
        audit_context,
    )
}

/// Performs necessary checks when the kernel generates and returns a file descriptor for BPF
/// maps.
pub(in crate::security) fn check_bpf_map_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    bpf_map: &BpfMap,
    flags: PermissionFlags,
) -> Result<(), Errno> {
    let audit_context = current_task.into();

    let subject_sid = task_effective_sid(current_task);
    let mut permissions = Vec::new();
    if flags.contains(PermissionFlags::READ) {
        permissions.push(BpfPermission::MapRead);
    }
    if flags.contains(PermissionFlags::WRITE) {
        permissions.push(BpfPermission::MapWrite);
    }
    for permission in permissions {
        check_permission(
            &security_server.as_permission_check(),
            current_task.kernel(),
            subject_sid,
            bpf_map.security_state.state.sid,
            permission,
            audit_context,
        )?;
    }
    Ok(())
}

/// Performs necessary checks when the kernel generates and returns a file descriptor for BPF
/// programs.
pub fn check_bpf_prog_access(
    security_server: &SecurityServer,
    current_task: &CurrentTask,
    bpf_program: &Program,
) -> Result<(), Errno> {
    let audit_context = current_task.into();

    let subject_sid = task_effective_sid(current_task);
    check_permission(
        &security_server.as_permission_check(),
        current_task.kernel(),
        subject_sid,
        bpf_program.security_state.state.sid,
        BpfPermission::ProgRun,
        audit_context,
    )
}
