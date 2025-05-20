// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use ffeedback::FileReportResults;
use fidl::prelude::*;
use fuchsia_component_test::LocalComponentHandles;
use futures::channel::mpsc::{self};
use futures::future::BoxFuture;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use std::sync::Arc;
use vfs::execution_scope::ExecutionScope;
use {fidl_fuchsia_boot as fboot, fidl_fuchsia_feedback as ffeedback, fidl_fuchsia_io as fio};

/// Identifier for ramdisk storage. Defined in sdk/lib/zbi-format/include/lib/zbi-format/zbi.h.
const ZBI_TYPE_STORAGE_RAMDISK: u32 = 0x4b534452;

pub async fn new_mocks(
    netboot: bool,
    vmo: Option<zx::Vmo>,
    crash_reports_sink: mpsc::Sender<ffeedback::CrashReport>,
) -> impl Fn(LocalComponentHandles) -> BoxFuture<'static, Result<(), Error>> + Sync + Send + 'static
{
    let vmo = vmo.map(Arc::new);
    let mock = move |handles: LocalComponentHandles| {
        let vmo_clone = vmo.clone();
        run_mocks(handles, netboot, vmo_clone, crash_reports_sink.clone()).boxed()
    };

    mock
}

async fn run_mocks(
    handles: LocalComponentHandles,
    netboot: bool,
    vmo: Option<Arc<zx::Vmo>>,
    crash_reports_sink: mpsc::Sender<ffeedback::CrashReport>,
) -> Result<(), Error> {
    let export = vfs::pseudo_directory! {
        "boot" => vfs::pseudo_directory! {
            "config" => vfs::pseudo_directory! {
                // Tests are expected to use a null zxcrypt policy.
                "zxcrypt" => vfs::file::read_only("null"),
            },
        },
        "svc" => vfs::pseudo_directory! {
            fboot::ArgumentsMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                run_boot_args(stream, netboot)
            }),
            fboot::ItemsMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                let vmo_clone = vmo.clone();
                run_boot_items(stream, vmo_clone)
            }),
            ffeedback::CrashReporterMarker::PROTOCOL_NAME => vfs::service::host(move |stream| {
                run_crash_reporter(stream, crash_reports_sink.clone())
            }),
        },
    };

    let scope = ExecutionScope::new();
    vfs::directory::serve_on(export, fio::PERM_READABLE, scope.clone(), handles.outgoing_dir);
    scope.wait().await;

    Ok(())
}

/// fshost uses exactly one boot item - it checks to see if there is an item of type
/// ZBI_TYPE_STORAGE_RAMDISK. If it's there, it's a vmo that represents a ramdisk version of the
/// fvm, and fshost creates a ramdisk from the vmo so it can go through the normal device matching.
async fn run_boot_items(mut stream: fboot::ItemsRequestStream, vmo: Option<Arc<zx::Vmo>>) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            fboot::ItemsRequest::Get { type_, extra, responder } => {
                assert_eq!(type_, ZBI_TYPE_STORAGE_RAMDISK);
                assert_eq!(extra, 0);
                let response_vmo = vmo.as_ref().map(|vmo| {
                    vmo.create_child(zx::VmoChildOptions::SLICE, 0, vmo.get_size().unwrap())
                        .unwrap()
                });
                responder.send(response_vmo, 0).unwrap();
            }
            fboot::ItemsRequest::Get2 { type_, extra, responder } => {
                assert_eq!(type_, ZBI_TYPE_STORAGE_RAMDISK);
                assert_eq!((*extra.unwrap()).n, 0);
                responder.send(Ok(Vec::new())).unwrap();
            }
            fboot::ItemsRequest::GetBootloaderFile { .. } => {
                panic!(
                    "unexpectedly called GetBootloaderFile on {}",
                    fboot::ItemsMarker::PROTOCOL_NAME
                );
            }
        }
    }
}

/// fshost expects a set of string and bool arguments to be available. This is a list of all the
/// arguments it looks for. NOTE: For what we are currently testing for, none of these are required,
/// so for now we either return None or the provided default depending on the context.
///
/// String args -
///   factory_verity_seal - only used when writing to the factory partition
/// Bool args -
///   netsvc.netboot (optional; default false)
async fn run_boot_args(mut stream: fboot::ArgumentsRequestStream, netboot: bool) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            fboot::ArgumentsRequest::GetString { key: _, responder } => {
                responder.send(None).unwrap();
            }
            fboot::ArgumentsRequest::GetStrings { keys, responder } => {
                responder.send(&vec![None; keys.len()]).unwrap();
            }
            fboot::ArgumentsRequest::GetBool { key: _, defaultval, responder } => {
                responder.send(defaultval).unwrap();
            }
            fboot::ArgumentsRequest::GetBools { keys, responder } => {
                let vec: Vec<_> = keys
                    .iter()
                    .map(|bool_pair| {
                        if bool_pair.key == "netsvc.netboot".to_string() && netboot {
                            true
                        } else {
                            bool_pair.defaultval
                        }
                    })
                    .collect();
                responder.send(&vec).unwrap();
            }
            fboot::ArgumentsRequest::Collect { .. } => {
                // This seems to be deprecated. Either way, fshost doesn't use it.
                panic!("unexpectedly called Collect on {}", fboot::ArgumentsMarker::PROTOCOL_NAME);
            }
        }
    }
}

async fn run_crash_reporter(
    mut stream: ffeedback::CrashReporterRequestStream,
    mut crash_reports_sink: mpsc::Sender<ffeedback::CrashReport>,
) {
    while let Some(request) = stream.next().await {
        match request.unwrap() {
            ffeedback::CrashReporterRequest::FileReport { report, responder } => {
                crash_reports_sink.send(report).await.unwrap();
                responder.send(Ok(&FileReportResults::default())).unwrap();
            }
        }
    }
}
