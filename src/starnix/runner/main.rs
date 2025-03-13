// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error};
use fuchsia_component::client::connect_to_protocol_sync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use kernel_manager::kernels::Kernels;
use kernel_manager::serve_starnix_manager;
use kernel_manager::suspend::SuspendContext;
use log::{error, info, warn};
use std::sync::Arc;
use {
    fidl_fuchsia_component_runner as frunner, fidl_fuchsia_settings as fsettings,
    fidl_fuchsia_starnix_runner as fstarnixrunner, zx,
};

enum Services {
    ComponentRunner(frunner::ComponentRunnerRequestStream),
    StarnixManager(fstarnixrunner::ManagerRequestStream),
}

const MEMORY_ROLE_NAME: &str = "fuchsia.starnix.runner";

#[fuchsia::main(logging_tags = ["starnix_runner"])]
async fn main() -> Result<(), Error> {
    if let Err(e) = fuchsia_scheduler::set_role_for_root_vmar(MEMORY_ROLE_NAME) {
        warn!(e:%; "failed to set memory role");
    }
    fuchsia_trace_provider::trace_provider_create_with_fdio();
    let config = starnix_runner_config::Config::take_from_startup_handle();
    if config.enable_data_collection {
        info!("Attempting to set user data sharing consent.");
        if let Ok(privacy) = connect_to_protocol_sync::<fsettings::PrivacyMarker>() {
            let privacy_settings = fsettings::PrivacySettings {
                user_data_sharing_consent: Some(true),
                ..Default::default()
            };
            match privacy.set(&privacy_settings, zx::MonotonicInstant::INFINITE) {
                Ok(Ok(())) => info!("Successfully set user data sharing consent."),
                Ok(Err(err)) => warn!("Could not set user data sharing consent: {err:?}"),
                Err(err) => warn!("Could not set user data sharing consent: {err:?}"),
            }
        } else {
            warn!("failed to connect to fuchsia.settings.Privacy");
        }
    }

    let kernels = Kernels::new();
    let mut fs = ServiceFs::new_local();

    let (sender, receiver) = async_channel::unbounded();
    kernel_manager::run_proxy_thread(receiver);

    fs.dir("svc").add_fidl_service(Services::ComponentRunner);
    fs.dir("svc").add_fidl_service(Services::StarnixManager);
    fs.take_and_serve_directory_handle()?;
    let suspend_context = Arc::new(SuspendContext::default());
    fs.for_each_concurrent(None, |request: Services| async {
        match request {
            Services::ComponentRunner(stream) => {
                if let Err(e) = serve_component_runner(stream, &kernels).await {
                    error!(e:%; "failed to serve component runner");
                }
            }
            Services::StarnixManager(stream) => {
                if let Err(e) =
                    serve_starnix_manager(stream, suspend_context.clone(), &kernels, &sender).await
                {
                    error!(e:%; "failed to serve starnix manager");
                }
            }
        }
    })
    .await;
    Ok(())
}

async fn serve_component_runner(
    mut stream: frunner::ComponentRunnerRequestStream,
    kernels: &Kernels,
) -> Result<(), Error> {
    while let Some(event) = stream.next().await {
        match event.context("serving component runner")? {
            frunner::ComponentRunnerRequest::Start { start_info, controller, .. } => {
                kernels.start(start_info, controller).await?;
            }
            frunner::ComponentRunnerRequest::_UnknownMethod { ordinal, .. } => {
                warn!(ordinal:%; "Unknown ComponentRunner request");
            }
        }
    }
    Ok(())
}
