// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use argh::FromArgs;
use component_debug::dirs::{connect_to_instance_protocol, OpenDirType};
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_device::ControllerMarker;
use fidl_fuchsia_fs_realm as fs_realm;
use fidl_fuchsia_sys2::RealmQueryMarker;
use fuchsia_component::client::{connect_channel_to_protocol_at_path, connect_to_protocol_at_path};

#[derive(FromArgs)]
#[argh(description = "A utility for checking the consistency of a filesystem instance running on a
    block device.")]
struct Options {
    /// the device path of the block device
    #[argh(positional)]
    device_path: String,

    /// the name of the filesystem formatted on the block device
    #[argh(positional)]
    filesystem: String,
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let opt: Options = argh::from_env();
    let device_path = opt.device_path;
    let filesystem = opt.filesystem;

    let (client_end, server_end) = create_endpoints::<ControllerMarker>();
    connect_channel_to_protocol_at_path(server_end.into_channel(), &device_path)?;

    // Connect to fs_realm
    const REALM_QUERY_SERVICE_PATH: &str = "/svc/fuchsia.sys2.RealmQuery.root";
    let realm_query_proxy =
        connect_to_protocol_at_path::<RealmQueryMarker>(REALM_QUERY_SERVICE_PATH)?;
    let moniker = "./core/fs_realm".try_into().unwrap();
    let fs_realm_proxy = connect_to_instance_protocol::<fs_realm::ControllerMarker>(
        &moniker,
        OpenDirType::Exposed,
        &realm_query_proxy,
    )
    .await?;

    fs_realm_proxy
        .check(client_end, &filesystem)
        .await
        .context("Transport error on fsck")?
        .map_err(zx::Status::from_raw)
        .context("Failed to fsck block device")?;
    Ok(())
}
