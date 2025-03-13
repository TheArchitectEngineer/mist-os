// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fidl::endpoints::Proxy;
use std::sync::LazyLock;
use test_runners_elf_lib::launcher::ComponentLauncher;
use test_runners_lib::elf::{Component, KernelError};
use test_runners_lib::errors::*;
use test_runners_lib::launch;
use test_runners_lib::logs::LoggerStream;
use zx::HandleBased;
use {fidl_fuchsia_fuzzer as fuzzer, fidl_fuchsia_process as fproc, fuchsia_runtime as runtime};

static VDSO_VMO: LazyLock<zx::Handle> = LazyLock::new(|| {
    runtime::take_startup_handle(runtime::HandleInfo::new(runtime::HandleType::VdsoVmo, 0))
        .expect("failed to take vDSO handle")
});

#[derive(Default)]
pub struct FuzzComponentLauncher {}

#[async_trait]
impl ComponentLauncher for FuzzComponentLauncher {
    /// Convenience wrapper around [`launch::launch_process`].
    async fn launch_process(
        &self,
        component: &Component,
        args: Vec<String>,
    ) -> Result<(zx::Process, launch::ScopedJob, LoggerStream, LoggerStream), RunTestError> {
        let mut args = args.clone();
        args.insert(0, component.url.clone());
        let registry = fuchsia_component::client::connect_to_protocol::<fuzzer::RegistrarMarker>()
            .map_err(launch::LaunchError::Launcher)?;
        let channel = registry.into_channel().expect("failed to take channel from proxy");
        let (loader_client, loader_server) = fidl::endpoints::create_endpoints();
        component.loader_service(loader_server);
        let executable_vmo = Some(component.executable_vmo()?);
        let result = launch::launch_process_with_separate_std_handles(launch::LaunchProcessArgs {
            bin_path: &component.binary,
            process_name: &component.name,
            job: Some(component.job.create_child_job().map_err(KernelError::CreateJob).unwrap()),
            ns: component.ns.clone(),
            args: Some(args),
            name_infos: None,
            environs: component.environ.clone(),
            handle_infos: Some(vec![
                fproc::HandleInfo {
                    handle: channel.into_zx_channel().into_handle(),
                    id: runtime::HandleInfo::new(runtime::HandleType::User0, 0).as_raw(),
                },
                fproc::HandleInfo {
                    handle: VDSO_VMO
                        .duplicate_handle(zx::Rights::SAME_RIGHTS)
                        .map_err(launch::LaunchError::DuplicateVdso)?,
                    id: runtime::HandleInfo::new(runtime::HandleType::VdsoVmo, 0).as_raw(),
                },
            ]),
            loader_proxy_chan: Some(loader_client.into_channel()),
            executable_vmo,
            options: component.options,
            config_vmo: component.config_vmo()?,
            url: Some(component.url.clone()),
            component_instance: component
                .component_instance
                .as_ref()
                .map(|c| c.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
        })
        .await?;
        Ok(result)
    }
}

impl FuzzComponentLauncher {
    pub fn new() -> Self {
        Self {}
    }
}
