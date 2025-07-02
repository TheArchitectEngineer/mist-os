// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::ContainerStartInfo;
use anyhow::{anyhow, bail, Context, Error};
use bstr::ByteVec;
use fidl_fuchsia_io as fio;
use starnix_core::fs::fuchsia::{create_remotefs_filesystem, RemoteBundle};
use starnix_core::fs::tmpfs::TmpFs;
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::fs_args::MountParams;
use starnix_core::vfs::{FileSystemHandle, FileSystemOptions, FsString};
use starnix_sync::{Locked, Unlocked};
use starnix_uapi::mount_flags::MountFlags;
use std::sync::Arc;

pub struct MountAction {
    pub path: FsString,
    pub fs: FileSystemHandle,
    pub flags: MountFlags,
}

impl MountAction {
    pub fn new_for_root(
        _locked: &mut Locked<Unlocked>,
        kernel: &Arc<Kernel>,
        pkg: &fio::DirectorySynchronousProxy,
        spec: &str,
    ) -> Result<MountAction, Error> {
        let (spec, options) = MountSpec::parse(spec)?;
        assert_eq!(spec.mount_point.as_slice(), b"/");
        let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;

        // We only support mounting these file systems at the root.
        // The root file system needs to be creatable without a task because we mount the root
        // file system before creating the initial task.
        let fs = match spec.fs_type.as_slice() {
            b"remote_bundle" => RemoteBundle::new_fs(kernel, pkg, options, rights)?,
            b"remotefs" => create_remotefs_filesystem(kernel, pkg, options, rights)?,
            b"tmpfs" => TmpFs::new_fs_with_options(kernel, options)?,
            _ => bail!("unsupported root file system: {}", spec.fs_type),
        };

        Ok(spec.into_action(fs))
    }

    pub fn from_spec(
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        start_info: Option<&ContainerStartInfo>,
        pkg: &fio::DirectorySynchronousProxy,
        spec: &str,
    ) -> Result<MountAction, Error> {
        let (spec, mut options) = MountSpec::parse(spec)?;
        let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;

        let fs = match spec.fs_type.as_slice() {
            // The remote_bundle file system is available only via the mounts declaration in CML.
            b"remote_bundle" => RemoteBundle::new_fs(current_task.kernel(), pkg, options, rights)?,

            // When used in a mounts declaration in CML, remotefs is relative to the pkg directory,
            // which is different than when remotefs is used with the mount() syscall. In that case,
            // remotefs is relative to the data directory.
            b"remotefs" => create_remotefs_filesystem(current_task.kernel(), pkg, options, rights)?,

            // The remotedir file system is used to mount a directory capability from the container
            // namespace. Cannot be used with a component running inside a container. Cannot be used
            // with the mount() syscall.
            b"remotedir" => {
                let path_string: Vec<u8> = options.source.clone().into();
                let path_buf = path_string.into_path_buf_lossy();
                let dir_channel = start_info
                    .ok_or_else(|| anyhow!("remotedir can be used only with a container"))?
                    .container_namespace
                    .get_namespace_channel(path_buf.as_path())
                    .context("failed to get namespace channel")?;

                let dir_proxy = fio::DirectorySynchronousProxy::new(dir_channel);
                options.source = b".".into();
                create_remotefs_filesystem(current_task.kernel(), &dir_proxy, options, rights)?
            }
            _ => current_task.create_filesystem(locked, spec.fs_type.as_ref(), options)?,
        };

        Ok(spec.into_action(fs))
    }
}

struct MountSpec {
    mount_point: FsString,
    fs_type: FsString,
    flags: MountFlags,
}

impl MountSpec {
    fn parse(spec: &str) -> Result<(MountSpec, FileSystemOptions), Error> {
        let mut iter = spec.splitn(4, ':');
        let mount_point =
            iter.next().ok_or_else(|| anyhow!("mount point is missing from {:?}", spec))?;
        let fs_type = iter.next().ok_or_else(|| anyhow!("fs type is missing from {:?}", spec))?;
        let fs_src = match iter.next() {
            Some(src) if !src.is_empty() => src,
            _ => ".",
        };

        let mut params = MountParams::parse(iter.next().unwrap_or_default().into())?;
        let flags = params.remove_mount_flags();

        Ok((
            MountSpec { fs_type: fs_type.into(), mount_point: mount_point.into(), flags },
            FileSystemOptions {
                source: fs_src.into(),
                flags: flags & MountFlags::STORED_ON_FILESYSTEM,
                params,
            },
        ))
    }

    fn into_action(self, fs: FileSystemHandle) -> MountAction {
        MountAction { path: self.mount_point.into(), fs, flags: self.flags }
    }
}
