// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::task::CurrentTask;
use starnix_core::vfs::stub_empty_file::StubEmptyFile;
use starnix_core::vfs::{FileSystemHandle, FsNodeHandle, StaticDirectoryBuilder};
use starnix_logging::bug_ref;
use starnix_uapi::mode;

pub fn uid_io_directory(current_task: &CurrentTask, fs: &FileSystemHandle) -> FsNodeHandle {
    let mut dir = StaticDirectoryBuilder::new(fs);
    dir.entry(
        current_task,
        "stats",
        StubEmptyFile::new_node("/proc/uid_io/stats", bug_ref!("https://fxbug.dev/322893966")),
        mode!(IFREG, 0o444),
    );
    dir.build(current_task)
}
