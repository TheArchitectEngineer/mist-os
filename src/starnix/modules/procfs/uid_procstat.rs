// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::vfs::pseudo::simple_directory::SimpleDirectory;
use starnix_core::vfs::pseudo::stub_empty_file::StubEmptyFile;
use starnix_core::vfs::{FileSystemHandle, FsNodeHandle};
use starnix_logging::bug_ref;
use starnix_uapi::file_mode::mode;

pub fn uid_procstat_directory(fs: &FileSystemHandle) -> FsNodeHandle {
    let dir = SimpleDirectory::new();
    dir.edit(fs, |dir| {
        dir.entry(
            "set",
            StubEmptyFile::new_node(
                "/proc/uid_procstat/set",
                bug_ref!("https://fxbug.dev/322894041"),
            ),
            mode!(IFREG, 0o222),
        );
    });
    // TODO: Validate the mode bits are correct.
    dir.into_node(fs, 0o777)
}
