// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{anyhow, Context, Error, Result};
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{ChildOptions, LocalComponentHandles, RealmBuilder};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};
use futures::channel::mpsc;
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use {
    fidl_fuchsia_driver_development as fdd, fidl_fuchsia_driver_test as fdt,
    fidl_fuchsia_reloaddriver_test as ft, fuchsia_async as fasync,
};

const WAITER_NAME: &'static str = "waiter";

async fn waiter_serve(
    mut stream: ft::WaiterRequestStream,
    mut sender: mpsc::Sender<(String, String)>,
) {
    while let Some(ft::WaiterRequest::Ack { from_node, from_name, status, .. }) =
        stream.try_next().await.expect("Stream failed")
    {
        assert_eq!(status, zx::Status::OK.into_raw());
        sender.try_send((from_node, from_name)).expect("Sender failed")
    }
}

async fn waiter_component(
    handles: LocalComponentHandles,
    sender: mpsc::Sender<(String, String)>,
) -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(move |stream: ft::WaiterRequestStream| {
        fasync::Task::spawn(waiter_serve(stream, sender.clone())).detach()
    });
    fs.serve_connection(handles.outgoing_dir)?;
    Ok(fs.collect::<()>().await)
}

fn send_get_device_info_request(
    service: &fdd::ManagerProxy,
    device_filter: &[String],
    exact_match: bool,
) -> Result<fdd::NodeInfoIteratorProxy> {
    let (iterator, iterator_server) =
        fidl::endpoints::create_proxy::<fdd::NodeInfoIteratorMarker>();

    service
        .get_node_info(device_filter, iterator_server, exact_match)
        .context("FIDL call to get device info failed")?;

    Ok(iterator)
}

async fn get_device_info(
    service: &fdd::ManagerProxy,
    device_filter: &[String],
    exact_match: bool,
) -> Result<Vec<fdd::NodeInfo>> {
    let iterator = send_get_device_info_request(service, device_filter, exact_match)?;

    let mut device_infos = Vec::new();
    loop {
        let mut device_info =
            iterator.get_next().await.context("FIDL call to get device info failed")?;
        if device_info.len() == 0 {
            break;
        }
        device_infos.append(&mut device_info);
    }
    Ok(device_infos)
}

#[fuchsia::test]
async fn test_reload_target() -> Result<()> {
    let (sender, mut receiver) = mpsc::channel(1);

    // Create the RealmBuilder.
    let builder = RealmBuilder::new().await?;
    builder.driver_test_realm_setup().await?;
    let waiter = builder
        .add_local_child(
            WAITER_NAME,
            move |handles: LocalComponentHandles| {
                Box::pin(waiter_component(handles, sender.clone()))
            },
            ChildOptions::new(),
        )
        .await?;
    let offer = fuchsia_component_test::Capability::protocol::<ft::WaiterMarker>().into();
    let dtr_offers = vec![offer];

    builder.driver_test_realm_add_dtr_offers(&dtr_offers, (&waiter).into()).await?;
    // Build the Realm.
    let instance = builder.build().await?;

    // Start the DriverTestRealm.
    let args = fdt::RealmArgs {
        root_driver: Some("fuchsia-boot:///dtr#meta/root.cm".to_string()),
        dtr_offers: Some(dtr_offers),
        ..Default::default()
    };
    instance.driver_test_realm_start(args).await?;

    let driver_dev = instance.root.connect_to_protocol_at_exposed_dir::<fdd::ManagerMarker>()?;

    // This maps nodes to Option<Option<u64>>. The outer option is whether the node has been seen
    // yet (if composite parent we start with `Some` for this since we don't receive acks
    // from them). The inner option is the driver host koid.
    let mut nodes = HashMap::from([
        ("dev".to_string(), None),
        ("B".to_string(), Some(None)), // composite parent
        ("C".to_string(), None),
        ("D".to_string(), Some(None)), // composite parent
        ("E".to_string(), Some(None)), // composite parent
        ("F".to_string(), None),
        ("G".to_string(), Some(None)), // composite parent
        ("H".to_string(), None),
    ]);

    // First we want to wait for all the nodes.
    reloadtest_tools::wait_for_nodes(&mut nodes, &mut receiver).await?;

    // Now we collect their initial driver host koids.
    let device_infos = get_device_info(&driver_dev, &[], /* exact_match= */ true).await?;
    reloadtest_tools::validate_host_koids("init", device_infos, &mut nodes, vec![], None).await?;

    // Let's restart the target driver, composite b.
    let restart_result = driver_dev
        .restart_driver_hosts(
            "fuchsia-boot:///dtr#meta/reload_test_nested_composite_b.cm",
            fdd::RestartRematchFlags::empty(),
        )
        .await?;
    if restart_result.is_err() {
        return Err(anyhow!("Failed to restart reload_test_nested_composite_b."));
    }

    // These are the nodes that should be restarted.
    let mut nodes_after_restart = HashMap::from([
        ("H".to_string(), None),
        ("G".to_string(), Some(None)), // composite parent
        ("F".to_string(), None),
    ]);

    // Wait for them to come back again.
    reloadtest_tools::wait_for_nodes(&mut nodes_after_restart, &mut receiver).await?;

    // Collect the new driver host koids.
    // Ensure same koid if not one of the ones expected to restart.
    // Make sure the host koid has changed from before the restart for the nodes that should have
    // restarted.
    let device_infos = get_device_info(&driver_dev, &[], /* exact_match= */ true).await?;
    reloadtest_tools::validate_host_koids(
        "restart",
        device_infos,
        &mut nodes_after_restart,
        vec![&nodes],
        None,
    )
    .await?;

    Ok(())
}
