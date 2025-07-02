// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod args;

use crate::subcommands::node::common;

use anyhow::{anyhow, Result};
use args::ListNodeCommand;
use fidl_fuchsia_driver_development as fdd;
use prettytable::format::consts::FORMAT_CLEAN;
use prettytable::{cell, row, Table};
use std::io::Write;

pub async fn list_node(
    cmd: ListNodeCommand,
    writer: &mut dyn Write,
    driver_development_proxy: fdd::ManagerProxy,
) -> Result<()> {
    let nodes = fuchsia_driver_dev::get_device_info(&driver_development_proxy, &[], false).await?;
    let nodes = common::filter_nodes(nodes, cmd.filter)?;
    let with_style = termion::is_tty(&std::io::stdout());

    if nodes.len() > 0 {
        if !cmd.verbose {
            for node in nodes {
                writeln!(
                    writer,
                    "{}",
                    node.moniker.unwrap_or_else(|| "Moniker not found.".to_string())
                )?;
            }
        } else {
            let table = create_table(nodes, with_style);
            table.print(writer)?;
        }
    } else {
        if cmd.fail_on_missing {
            return Err(anyhow!("No nodes found."));
        } else {
            writeln!(writer, "No nodes found.")?;
        }
    }

    Ok(())
}

fn create_table(nodes: Vec<fdd::NodeInfo>, with_style: bool) -> Table {
    let mut table = Table::new();
    table.set_format(*FORMAT_CLEAN);
    table.set_titles(row!("State", "Moniker", "Owner"));

    for node in nodes {
        let moniker = node.moniker.unwrap_or_else(|| "Moniker not found.".to_string());
        let (state, owner) =
            common::get_state_and_owner(node.quarantined, &node.bound_driver_url, with_style);
        table.add_row(row!(state, moniker, owner));
    }
    table
}
