// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_component::client as fclient;
use log::*;
use zx::AsHandleRef;
use {fidl_fuchsia_test as ftest, fuchsia_runtime as fruntime};

#[fuchsia::main]
async fn main() {
    info!("started");

    let my_thread_koid =
        fruntime::with_thread_self(|thread| thread.get_koid().expect("failed to get thread koid"));
    let thread_koid_reporter_proxy =
        fclient::connect_to_protocol::<ftest::ThreadKoidReporterMarker>()
            .expect("failed to connect to thread koid reporter");
    thread_koid_reporter_proxy
        .report_my_thread_koid(my_thread_koid.raw_koid())
        .expect("failed to report thread koid");
    info!("thread koid successfully reported");

    panic!("time to crash!");
}
