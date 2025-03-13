// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::FileHandle;

use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;

mod remote;
mod remote_bundle;
#[cfg(not(feature = "starnix_lite"))]
qmod remote_unix_domain_socket;
mod remote_volume;
mod syslog;
mod timer;

pub mod sync_file;
pub mod zxio;

pub use remote::*;
pub use remote_bundle::RemoteBundle;
#[cfg(not(feature = "starnix_lite"))]
pub use remote_unix_domain_socket::*;
pub use remote_volume::*;
pub use syslog::*;
pub use timer::*;

/// Create a FileHandle from a zx::Handle.
pub fn create_file_from_handle(
    current_task: &CurrentTask,
    handle: zx::Handle,
) -> Result<FileHandle, Errno> {
    new_remote_file(current_task, handle, OpenFlags::RDWR)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::testing::*;
    use zx::HandleBased;

    #[::fuchsia::test]
    async fn test_create_from_invalid_handle() {
        let (_kernel, current_task) = create_kernel_and_task();
        assert!(create_file_from_handle(&current_task, zx::Handle::invalid()).is_err());
    }

    #[::fuchsia::test]
    async fn test_create_pipe_from_handle() {
        let (_kernel, current_task) = create_kernel_and_task();
        let (left_handle, right_handle) = zx::Socket::create_stream();
        create_file_from_handle(&current_task, left_handle.into_handle())
            .expect("failed to create left FileHandle");
        create_file_from_handle(&current_task, right_handle.into_handle())
            .expect("failed to create right FileHandle");
    }
}
