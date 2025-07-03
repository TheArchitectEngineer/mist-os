// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// This module tests ota_downloader.
use assert_matches::assert_matches;
use fidl_fuchsia_pkg as fpkg;
use fuchsia_pkg_testing::RepositoryBuilder;
use futures::StreamExt as _;
use lib::{make_pkg_with_extra_blobs, TestEnvBuilder, EMPTY_REPO_PATH};
use std::sync::Arc;

#[fuchsia::test]
async fn test_fetch_blob() {
    let pkg = make_pkg_with_extra_blobs("test-package", 10).await;
    let repo = Arc::new(
        RepositoryBuilder::from_template_dir(EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let env = TestEnvBuilder::new().build().await;
    let served_repository = repo.server().start().unwrap();
    let base_url = served_repository.local_url() + "/blobs";

    for blob in pkg.list_blobs() {
        let () = env.fetch_blob(blob.into(), &base_url).await.unwrap();
    }
    assert!(env.blobfs.list_blobs().unwrap().is_superset(&pkg.list_blobs()));

    env.stop().await;
}

#[fuchsia::test]
async fn test_fetch_blob_concurrent() {
    let pkg = make_pkg_with_extra_blobs("test-package", 100).await;
    let repo = Arc::new(
        RepositoryBuilder::from_template_dir(EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let env = TestEnvBuilder::new().build().await;
    let served_repository = repo.server().start().unwrap();
    let base_url = served_repository.local_url() + "/blobs";

    let () = futures::stream::iter(pkg.list_blobs())
        .for_each_concurrent(None, async |hash| {
            let () = env.fetch_blob(hash.into(), &base_url).await.unwrap();
        })
        .await;
    assert!(env.blobfs.list_blobs().unwrap().is_superset(&pkg.list_blobs()));

    env.stop().await;
}

#[fuchsia::test]
async fn test_fetch_blob_404() {
    let repo =
        Arc::new(RepositoryBuilder::from_template_dir(EMPTY_REPO_PATH).build().await.unwrap());
    let env = TestEnvBuilder::new().build().await;
    let served_repository = repo.server().start().unwrap();
    let base_url = served_repository.local_url() + "/blobs";

    assert_matches!(
        env.fetch_blob([0; 32].into(), &base_url).await,
        Err(fpkg::ResolveError::UnavailableBlob)
    );

    env.stop().await;
}
