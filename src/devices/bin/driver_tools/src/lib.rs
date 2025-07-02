// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;
mod common;
mod subcommands;

use anyhow::{Context, Result};
use args::{DriverCommand, DriverSubCommand};
use driver_connector::DriverConnector;
use std::io;

pub async fn driver(
    cmd: DriverCommand,
    driver_connector: impl DriverConnector,
    writer: &mut dyn io::Write,
) -> Result<()> {
    match cmd.subcommand {
        DriverSubCommand::Dump(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::dump::dump(subcmd, writer, driver_development_proxy)
                .await
                .context("Dump subcommand failed")?;
        }
        DriverSubCommand::List(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list::list(subcmd, writer, driver_development_proxy)
                .await
                .context("List subcommand failed")?;
        }
        DriverSubCommand::ListComposites(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_composites::list_composites(subcmd, writer, driver_development_proxy)
                .await
                .context("List composites subcommand failed")?;
        }
        DriverSubCommand::ListDevices(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_devices::list_devices(subcmd, driver_development_proxy)
                .await
                .context("List-devices subcommand failed")?;
        }
        DriverSubCommand::ListHosts(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_hosts::list_hosts(subcmd, driver_development_proxy)
                .await
                .context("List-hosts subcommand failed")?;
        }
        DriverSubCommand::ListCompositeNodeSpecs(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::list_composite_node_specs::list_composite_node_specs(
                subcmd,
                writer,
                driver_development_proxy,
            )
            .await
            .context("list-composite-node-specs subcommand failed")?;
        }
        DriverSubCommand::Register(subcmd) => {
            let driver_registrar_proxy = driver_connector
                .get_driver_registrar_proxy(subcmd.select)
                .await
                .context("Failed to get driver registrar proxy")?;
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::register::register(
                subcmd,
                writer,
                driver_registrar_proxy,
                driver_development_proxy,
            )
            .await
            .context("Register subcommand failed")?;
        }
        DriverSubCommand::Restart(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::restart::restart(subcmd, writer, driver_development_proxy)
                .await
                .context("Restart subcommand failed")?;
        }
        #[cfg(not(target_os = "fuchsia"))]
        DriverSubCommand::StaticChecks(subcmd) => {
            static_checks_lib::static_checks(subcmd, writer)
                .await
                .context("StaticChecks subcommand failed")?;
        }
        DriverSubCommand::TestNode(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::test_node::test_node(&subcmd, driver_development_proxy)
                .await
                .context("AddTestNode subcommand failed")?;
        }
        DriverSubCommand::Disable(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(subcmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::disable::disable(subcmd, writer, driver_development_proxy)
                .await
                .context("Disable subcommand failed")?;
        }
        DriverSubCommand::Node(subcmd) => {
            let driver_development_proxy = driver_connector
                .get_driver_development_proxy(cmd.select)
                .await
                .context("Failed to get driver development proxy")?;
            subcommands::node::node(subcmd, writer, driver_development_proxy)
                .await
                .context("Node subcommand failed")?;
        }
    };
    Ok(())
}
