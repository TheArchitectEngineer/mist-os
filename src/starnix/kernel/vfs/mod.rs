// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod anon_node;
mod dir_entry;
mod dirent_sink;
mod epoll;
mod fd_number;
mod fd_table;
mod file_object;
mod file_system;
mod file_write_guard;
mod fs_context;
mod fs_node;
mod memory_regular;
mod namespace;
mod record_locks;
mod splice;
mod symlink_node;
mod userfault_file;
mod wd_number;
mod xattr;

pub mod aio;
pub mod buffers;
pub mod crypt_service;
pub mod eventfd;
pub mod file_server;
pub mod fs_args;
pub mod fs_node_cache;
pub mod fs_registry;
pub mod fsverity;
pub mod inotify;
pub mod io_uring;
pub mod memory_directory;
pub mod path;
pub mod pidfd;
pub mod pipe;
pub mod pseudo;
pub mod rw_queue;
pub mod socket;
pub mod syscalls;
pub mod timer;

pub use anon_node::*;
pub use buffers::*;
pub use dir_entry::*;
pub use dirent_sink::*;
pub use epoll::*;
pub use fd_number::*;
pub use fd_table::*;
pub use file_object::*;
pub use file_system::*;
pub use file_write_guard::*;
pub use fs_context::*;
pub use fs_node::*;
pub use memory_directory::*;
pub use memory_regular::*;
pub use namespace::*;
pub use path::*;
pub use pidfd::*;
pub use record_locks::*;
pub use symlink_node::*;
pub use userfault_file::*;
pub use wd_number::*;
pub use xattr::*;

use crate::task::CurrentTask;
use starnix_lifecycle::{ObjectReleaser, ReleaserAction};
use starnix_sync::{FileOpsCore, Locked};
use starnix_types::ownership::{Releasable, ReleaseGuard};
use std::cell::RefCell;
use std::ops::DerefMut;
use std::sync::Arc;

/// Register the container to be deferred released.
pub fn register<T: for<'a> Releasable<Context<'a> = CurrentTaskAndLocked<'a>> + 'static>(
    to_release: T,
) {
    RELEASERS.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .expect("DelayedReleaser hasn't been finalized yet")
            .releasables
            .push(Box::new(Some(to_release)));
    });
}

impl<T> CurrentTaskAndLockedReleasable for Option<T>
where
    for<'a> T: Releasable<Context<'a> = CurrentTaskAndLocked<'a>>,
{
    fn release_with_context(&mut self, context: CurrentTaskAndLocked<'_>) {
        if let Some(this) = self.take() {
            <T as Releasable>::release(this, context);
        }
    }
}

pub enum FileObjectReleaserAction {}
impl ReleaserAction<FileObject> for FileObjectReleaserAction {
    fn release(file_object: ReleaseGuard<FileObject>) {
        register(file_object);
    }
}
pub type FileReleaser = ObjectReleaser<FileObject, FileObjectReleaserAction>;

pub enum FsNodeReleaserAction {}
impl ReleaserAction<FsNode> for FsNodeReleaserAction {
    fn release(fs_node: ReleaseGuard<FsNode>) {
        register(fs_node);
    }
}
pub type FsNodeReleaser = ObjectReleaser<FsNode, FsNodeReleaserAction>;

pub type CurrentTaskAndLocked<'a> = (&'a mut Locked<FileOpsCore>, &'a CurrentTask);

/// An object-safe/dyn-compatible trait to wrap `Releasable` types.
pub trait CurrentTaskAndLockedReleasable {
    fn release_with_context(&mut self, context: CurrentTaskAndLocked<'_>);
}

thread_local! {
    /// Container of all `FileObject` that are not used anymore, but have not been closed yet.
    pub static RELEASERS: RefCell<Option<LocalReleasers>> = RefCell::new(Some(LocalReleasers::default()));
}

#[derive(Default)]
pub struct LocalReleasers {
    /// The list of entities to be deferred released.
    pub releasables: Vec<Box<dyn CurrentTaskAndLockedReleasable>>,
}

impl LocalReleasers {
    fn is_empty(&self) -> bool {
        self.releasables.is_empty()
    }
}

impl Releasable for LocalReleasers {
    type Context<'a> = CurrentTaskAndLocked<'a>;

    fn release<'a>(self, context: CurrentTaskAndLocked<'a>) {
        let (locked, current_task) = context;
        for mut releasable in self.releasables {
            releasable.release_with_context((locked, current_task));
        }
    }
}

/// Service to handle delayed releases.
///
/// Delayed releases are cleanup code that is run at specific point where the lock level is
/// known. The starnix kernel must ensure that delayed releases are run regularly.
#[derive(Debug, Default)]
pub struct DelayedReleaser {}

impl DelayedReleaser {
    pub fn flush_file(&self, file: &FileHandle, id: FdTableId) {
        register(FlushedFile(Arc::clone(file), id));
    }

    /// Run all current delayed releases for the current thread.
    pub fn apply<'a>(&self, locked: &'a mut Locked<FileOpsCore>, current_task: &'a CurrentTask) {
        loop {
            let releasers = RELEASERS.with(|cell| {
                std::mem::take(
                    cell.borrow_mut()
                        .as_mut()
                        .expect("DelayedReleaser hasn't been finalized yet")
                        .deref_mut(),
                )
            });
            if releasers.is_empty() {
                return;
            }
            releasers.release((locked, current_task));
        }
    }

    /// Prevent any further releasables from being registered on this thread.
    ///
    /// This function should be called during thread teardown to ensure that we do not
    /// register any new releasables on this thread after we have finalized the delayed
    /// releasables for the last time.
    pub fn finalize() {
        RELEASERS.with(|cell| {
            assert!(cell
                .borrow()
                .as_ref()
                .expect("DelayedReleaser hasn't been finalized yet")
                .is_empty());
            *cell.borrow_mut() = None;
        });
    }
}

struct FlushedFile(FileHandle, FdTableId);

impl Releasable for FlushedFile {
    type Context<'a> = CurrentTaskAndLocked<'a>;
    fn release<'a>(self, context: Self::Context<'a>) {
        let (locked, current_task) = context;
        self.0.flush(locked, current_task, self.1);
    }
}
