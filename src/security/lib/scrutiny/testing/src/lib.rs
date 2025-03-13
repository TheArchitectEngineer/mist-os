// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod artifact;
pub mod fake;

use fuchsia_url::RepositoryUrl;

pub static TEST_REPO_URL: std::sync::LazyLock<RepositoryUrl> =
    std::sync::LazyLock::new(|| RepositoryUrl::parse_host("test.fuchsia.com".to_string()).unwrap());
