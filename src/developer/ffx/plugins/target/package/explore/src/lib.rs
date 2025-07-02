// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use ffx_target_package_explore_args::ExploreCommand;
use ffx_writer::VerifiedMachineWriter;
use fho::{bug, exit_with_code, user_error, Error, FfxContext as _, FfxMain, FfxTool, Result};
use fidl_fuchsia_dash as fdash;
use futures::stream::StreamExt as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use target_holders::moniker;

#[derive(Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Successfully completed session.
    Ok {},
    /// Unexpected error with string denoting error message.
    UnexpectedError { message: String },
    /// A known error that can be reported to the user.
    UserError { message: String },
}

#[derive(FfxTool)]
pub struct ExploreTool {
    #[command]
    cmd: ExploreCommand,
    #[with(fho::deferred(moniker("/core/debug-dash-launcher")))]
    dash_launcher_proxy: fho::Deferred<fdash::LauncherProxy>,
}

fho::embedded_plugin!(ExploreTool);

#[async_trait::async_trait(?Send)]
impl FfxMain for ExploreTool {
    type Writer = VerifiedMachineWriter<CommandStatus>;
    async fn main(self, mut writer: Self::Writer) -> Result<()> {
        #[allow(clippy::large_futures)]
        match explore_cmd(self.cmd, self.dash_launcher_proxy.await?).await {
            Ok(()) => {
                writer.machine(&CommandStatus::Ok {})?;
                Ok(())
            }
            Err(e @ Error::User(_)) => {
                writer.machine(&CommandStatus::UserError { message: e.to_string() })?;
                Err(e)
            }
            Err(e) => {
                writer.machine(&CommandStatus::UnexpectedError { message: e.to_string() })?;
                Err(e)
            }
        }
    }
}

async fn explore_cmd(cmd: ExploreCommand, dash_launcher: fdash::LauncherProxy) -> Result<()> {
    let ExploreCommand { url, subpackages, tools, command, fuchsia_pkg_resolver } = cmd;

    let (client, server) = fidl::Socket::create_stream();
    let stdout = if command.is_some() {
        socket_to_stdio::Stdout::buffered()
    } else {
        socket_to_stdio::Stdout::raw()?
    };

    let () = dash_launcher
        .explore_package_over_socket2(
            fuchsia_pkg_resolver,
            &url,
            &subpackages,
            server,
            &tools,
            command.as_deref(),
        )
        .await
        .bug_context("fidl error launching dash")?
        .map_err(|e| match e {
            fdash::LauncherError::ResolveTargetPackage => {
                user_error!("No package found matching '{}' {}.", url, subpackages.join(" "))
            }
            e => bug!("Unexpected error launching dash: {:?}", e),
        })?;

    #[allow(clippy::large_futures)]
    let () = socket_to_stdio::connect_socket_to_stdio(client, stdout).await?;

    let exit_code = wait_for_shell_exit(&dash_launcher).await?;
    exit_with_code!(exit_code);
}

async fn wait_for_shell_exit(launcher_proxy: &fdash::LauncherProxy) -> fho::Result<i32> {
    // Report process errors and return the exit status.
    let mut event_stream = launcher_proxy.take_event_stream();
    match event_stream.next().await {
        Some(Ok(fdash::LauncherEvent::OnTerminated { return_code })) => Ok(return_code),
        Some(Err(e)) => Err(user_error!("OnTerminated event error: {:?}", e)),
        None => Err(user_error!("didn't receive an expected OnTerminated event")),
    }
}
