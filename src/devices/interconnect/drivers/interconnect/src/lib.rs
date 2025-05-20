// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::graph::{NodeGraph, NodeId, Path, PathId};
use fdf_component::{
    driver_register, Driver, DriverContext, Node, NodeBuilder, ZirconServiceOffer,
};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_driver_framework::NodeControllerMarker;
use fidl_fuchsia_hardware_interconnect as icc;
use fuchsia_component::server::ServiceFs;
use fuchsia_sync::Mutex;
use futures::{StreamExt, TryStreamExt};
use log::{error, warn};
use std::collections::BTreeMap;
use std::sync::Arc;
use zx::Status;

mod graph;

driver_register!(InterconnectDriver);

struct Child {
    /// List of nodes following directed path from start of path to end of path.
    path: Path,
    /// Directed graph which stores all nodes and bandwidth requests for each of their incoming
    /// edges.
    graph: Arc<Mutex<NodeGraph>>,
    #[allow(unused)]
    controller: ClientEnd<NodeControllerMarker>,
    device: icc::DeviceProxy,
}

impl Child {
    async fn set_bandwidth(
        &self,
        average_bandwidth_bps: Option<u64>,
        peak_bandwidth_bps: Option<u64>,
    ) -> Result<(), Status> {
        let average_bandwidth_bps = average_bandwidth_bps.ok_or(Status::INVALID_ARGS)?;
        let peak_bandwidth_bps = peak_bandwidth_bps.ok_or(Status::INVALID_ARGS)?;

        let requests = {
            let mut graph = self.graph.lock();
            graph.update_path(&self.path, average_bandwidth_bps, peak_bandwidth_bps);
            graph.make_bandwidth_requests(&self.path)
        };

        self.device
            .set_nodes_bandwidth(&requests)
            .await
            .map_err(|err| {
                error!("Failed to set bandwidth with {err}");
                Status::INTERNAL
            })?
            .map_err(Status::from_raw)?;

        // TODO(b/405206028): On failure, try to set old values?

        Ok(())
    }

    async fn run_path_server(&self, mut service: icc::PathRequestStream) {
        use icc::PathRequest::*;
        while let Some(req) = service.try_next().await.unwrap() {
            match req {
                SetBandwidth { payload, responder, .. } => responder.send(
                    self.set_bandwidth(payload.average_bandwidth_bps, payload.peak_bandwidth_bps)
                        .await
                        .map_err(Status::into_raw),
                ),
                // Ignore unknown requests.
                _ => {
                    warn!("Received unknown path request");
                    Ok(())
                }
            }
            .unwrap();
        }
    }
}

#[allow(unused)]
struct InterconnectDriver {
    node: Node,
    children: Arc<BTreeMap<String, Child>>,
    scope: fuchsia_async::Scope,
}

impl Driver for InterconnectDriver {
    const NAME: &str = "interconnect";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        let node = context.take_node()?;

        let device = context
            .incoming
            .service_marker(icc::ServiceMarker)
            .connect()?
            .connect_to_device()
            .map_err(|err| {
                error!("Error connecting to interconnect device at driver startup: {err}");
                Status::INTERNAL
            })?;

        let (nodes, edges) = device.get_node_graph().await.map_err(|err| {
            error!("Failed to get node graph with {err}");
            Status::INTERNAL
        })?;
        let mut graph = NodeGraph::new(nodes, edges)?;

        let path_endpoints = device.get_path_endpoints().await.map_err(|err| {
            error!("Failed to get path endpoints with {err}");
            Status::INTERNAL
        })?;
        let paths: Vec<_> = Result::from_iter(path_endpoints.into_iter().map(|path| {
            let path_id = PathId(path.id.ok_or(Status::INVALID_ARGS)?);
            let path_name = path.name.ok_or(Status::INVALID_ARGS)?;
            let src_node_id = NodeId(path.src_node_id.ok_or(Status::INVALID_ARGS)?);
            let dst_node_id = NodeId(path.dst_node_id.ok_or(Status::INVALID_ARGS)?);
            Ok::<_, Status>(graph.make_path(path_id, path_name, src_node_id, dst_node_id)?)
        }))?;

        let mut outgoing = ServiceFs::new();

        let graph = Arc::new(Mutex::new(graph));
        let mut children = BTreeMap::new();
        for path in paths {
            let name = format!("{}-{}", path.name(), path.id());
            let name_clone = name.clone();
            let offer = ZirconServiceOffer::new()
                .add_default_named(&mut outgoing, &name, move |req| {
                    let icc::PathServiceRequest::Path(service) = req;
                    (service, name_clone.clone())
                })
                .build();

            let node_args = NodeBuilder::new(&name)
                .add_property(bind_fuchsia::BIND_INTERCONNECT_PATH_ID, path.id().0)
                .add_offer(offer)
                .build();
            let controller = node.add_child(node_args).await?;
            let graph = graph.clone();
            let device = device.clone();
            children.insert(name.clone(), Child { path, graph, controller, device });
        }
        // TODO(b/405206028): Initialize all nodes to initial bus bandwidths.

        context.serve_outgoing(&mut outgoing)?;

        let children = Arc::new(children);

        let scope = fuchsia_async::Scope::new_with_name("outgoing_directory");
        let children_clone = children.clone();
        scope.spawn_local(async move {
            outgoing
                .for_each_concurrent(None, move |(request, child_name)| {
                    let children = children_clone.clone();
                    async move {
                        if let Some(node) = children.get(&child_name) {
                            node.run_path_server(request).await;
                        } else {
                            error!("Failed to find child {child_name}");
                        }
                    }
                })
                .await;
        });

        Ok(Self { node, children, scope })
    }

    async fn stop(&self) {}
}
