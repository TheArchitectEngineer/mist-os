// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{Incoming, Node};
use fuchsia_component::server::{ServiceFs, ServiceObjTrait};
use log::error;
use namespace::Namespace;
use zx::Status;

use fdf::DispatcherRef;
use fidl_fuchsia_driver_framework::DriverStartArgs;

/// The context arguments passed to the driver in its start arguments.
pub struct DriverContext {
    /// A reference to the root [`fdf::Dispatcher`] for this driver.
    pub root_dispatcher: DispatcherRef<'static>,
    /// The original [`DriverStartArgs`] passed in as start arguments, minus any parts that were
    /// used to construct other elements of [`Self`].
    pub start_args: DriverStartArgs,
    /// The incoming namespace constructed from [`DriverStartArgs::incoming`]. Since producing this
    /// consumes the incoming namespace from [`Self::start_args`], that will always be [`None`].
    pub incoming: Incoming,
    #[doc(hidden)]
    _private: (),
}

impl DriverContext {
    /// Binds the node proxy client end from the start args into a [`NodeProxy`] that can be used
    /// to add child nodes. Dropping this proxy will result in the node being removed and the
    /// driver starting shutdown, so it should be bound and stored in your driver object in its
    /// [`crate::Driver::start`] method.
    ///
    /// After calling this, [`DriverStartArgs::node`] in [`Self::start_args`] will be `None`.
    ///
    /// Returns [`Status::INVALID_ARGS`] if the node client end is not present in the start
    /// arguments.
    pub fn take_node(&mut self) -> Result<Node, Status> {
        let node_client = self.start_args.node.take().ok_or(Status::INVALID_ARGS)?;
        Ok(Node::from(node_client.into_proxy()))
    }

    /// Serves the given [`ServiceFs`] on the node's outgoing directory. This can only be called
    /// once, and after this the [`DriverStartArgs::outgoing_dir`] member will be [`None`].
    ///
    /// Logs an error and returns [`Status::INVALID_ARGS`] if the outgoing directory server end is
    /// not present in the start arguments, or [`Status::INTERNAL`] if serving the connection
    /// failed.
    pub fn serve_outgoing<O: ServiceObjTrait>(
        &mut self,
        outgoing_fs: &mut ServiceFs<O>,
    ) -> Result<(), Status> {
        let Some(outgoing_dir) = self.start_args.outgoing_dir.take() else {
            error!("Tried to serve on outgoing directory but it wasn't available");
            return Err(Status::INVALID_ARGS);
        };
        outgoing_fs.serve_connection(outgoing_dir).map_err(|err| {
            error!("Failed to serve outgoing directory: {err}");
            Status::INTERNAL
        })?;

        Ok(())
    }

    pub(crate) fn new(
        root_dispatcher: DispatcherRef<'static>,
        mut start_args: DriverStartArgs,
    ) -> Result<Self, Status> {
        let incoming_namespace: Namespace = start_args
            .incoming
            .take()
            .unwrap_or_else(|| vec![])
            .try_into()
            .map_err(|_| Status::INVALID_ARGS)?;
        let incoming = incoming_namespace.try_into().map_err(|_| Status::INVALID_ARGS)?;
        Ok(DriverContext { root_dispatcher, start_args, incoming, _private: () })
    }

    pub(crate) fn start_logging(&self, driver_name: &str) -> Result<(), Status> {
        let log_proxy = match self.incoming.protocol().connect() {
            Ok(log_proxy) => log_proxy,
            Err(err) => {
                eprintln!("Error connecting to log sink proxy at driver startup: {err}. Continuing without logging.");
                return Ok(());
            }
        };

        if let Err(e) = diagnostics_log::initialize(
            diagnostics_log::PublishOptions::default()
                .use_log_sink(log_proxy)
                .tags(&["driver", driver_name]),
        ) {
            eprintln!("Error initializing logging at driver startup: {e}");
        }
        Ok(())
    }
}
