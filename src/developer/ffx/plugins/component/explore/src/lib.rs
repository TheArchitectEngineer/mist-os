// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use component_debug::cli::explore_cmd;
use errors::ffx_error;
use ffx_component::rcs::connect_to_realm_query;
use ffx_component_explore_args::ExploreComponentCommand;
use ffx_writer::SimpleWriter;
use fho::{FfxMain, FfxTool};
use fidl_fuchsia_dash::LauncherProxy;
use socket_to_stdio::Stdout;
use target_holders::{moniker, RemoteControlProxyHolder};

#[derive(FfxTool)]
pub struct ExploreTool {
    #[command]
    cmd: ExploreComponentCommand,
    rcs: RemoteControlProxyHolder,
    #[with(moniker("/core/debug-dash-launcher"))]
    dash_launcher: LauncherProxy,
}

fho::embedded_plugin!(ExploreTool);

// TODO(https://fxbug.dev/42053815): This plugin needs E2E tests.
#[async_trait(?Send)]
impl FfxMain for ExploreTool {
    type Writer = SimpleWriter;

    async fn main(self, _writer: Self::Writer) -> fho::Result<()> {
        let realm_query = connect_to_realm_query(&self.rcs).await?;
        let stdout = if self.cmd.command.is_some() { Stdout::buffered() } else { Stdout::raw()? };

        // All errors from component_debug library are user-visible.
        #[allow(clippy::large_futures)]
        explore_cmd(
            self.cmd.query,
            self.cmd.ns_layout,
            self.cmd.command,
            self.cmd.tools,
            self.dash_launcher,
            realm_query,
            stdout,
        )
        .await
        .map_err(|e| ffx_error!(e))?;
        Ok(())
    }
}
