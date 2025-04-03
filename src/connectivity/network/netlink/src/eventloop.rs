// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::debug_assert;
use std::convert::Infallible as Never;
use std::pin::pin;

use anyhow::{Context as _, Error};
use assert_matches::assert_matches;
use derivative::Derivative;
use futures::channel::{mpsc, oneshot};
use futures::stream::BoxStream;
use futures::{FutureExt as _, StreamExt as _};
use linux_uapi::rtnetlink_groups_RTNLGRP_ND_USEROPT;
use net_types::ip::{Ip, IpInvariant, Ipv4, Ipv6};
use {
    fidl_fuchsia_net_interfaces as fnet_interfaces,
    fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext, fidl_fuchsia_net_ndp as fnet_ndp,
    fidl_fuchsia_net_root as fnet_root, fidl_fuchsia_net_routes as fnet_routes,
    fidl_fuchsia_net_routes_admin as fnet_routes_admin,
    fidl_fuchsia_net_routes_ext as fnet_routes_ext,
};

use crate::client::{AsyncWorkItem, ClientTable};
use crate::logging::{log_debug, log_info};
use crate::messaging::Sender;
use crate::multicast_groups::ModernGroup;
use crate::netlink_packet::errno::Errno;
use crate::protocol_family::route::NetlinkRoute;
use crate::protocol_family::ProtocolFamily;
use crate::{interfaces, route_tables, routes, rules, FeatureFlags, NetlinkRouteNotifiedGroup};

#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub(crate) enum UnifiedRequest<S: Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>> {
    InterfacesRequest(interfaces::Request<S>),
    RoutesV4Request(routes::Request<S, Ipv4>),
    RoutesV6Request(routes::Request<S, Ipv6>),
    RuleV4Request(rules::RuleRequest<S, Ipv4>, oneshot::Sender<Result<(), Errno>>),
    RuleV6Request(rules::RuleRequest<S, Ipv6>, oneshot::Sender<Result<(), Errno>>),
}

impl<S: Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>, I: Ip> From<routes::Request<S, I>>
    for UnifiedRequest<S>
{
    fn from(request: routes::Request<S, I>) -> Self {
        I::map_ip_in(request, UnifiedRequest::RoutesV4Request, UnifiedRequest::RoutesV6Request)
    }
}

impl<S: Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>> UnifiedRequest<S> {
    pub(crate) fn rule_request<I: Ip>(
        request: rules::RuleRequest<S, I>,
        sender: oneshot::Sender<Result<(), Errno>>,
    ) -> Self {
        I::map_ip_in(
            (request, IpInvariant(sender)),
            |(request, IpInvariant(sender))| UnifiedRequest::RuleV4Request(request, sender),
            |(request, IpInvariant(sender))| UnifiedRequest::RuleV6Request(request, sender),
        )
    }
}

pub(crate) enum UnifiedEvent {
    RoutesV4Event(fnet_routes_ext::Event<Ipv4>),
    RoutesV6Event(fnet_routes_ext::Event<Ipv6>),
    InterfacesEvent(fnet_interfaces_ext::EventWithInterest<fnet_interfaces_ext::AllInterest>),
}

#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub(crate) enum UnifiedPendingRequest<S: Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>> {
    RoutesV4(crate::routes::PendingRouteRequest<S, Ipv4>),
    RoutesV6(crate::routes::PendingRouteRequest<S, Ipv6>),
    Interfaces(crate::interfaces::PendingRequest<S>),
}

/// Contains the asynchronous work related to routes and interfaces. Creates
/// routes and interface hanging get watchers and connects to route and
/// interface administration protocols in order to single-threadedly service
/// incoming `UnifiedRequest`s.
pub(crate) struct EventLoop<
    H,
    S: crate::messaging::Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>,
> {
    pub(crate) interfaces_proxy: fnet_root::InterfacesProxy,
    pub(crate) interfaces_state_proxy: fnet_interfaces::StateProxy,
    pub(crate) v4_routes_state: fnet_routes::StateV4Proxy,
    pub(crate) v6_routes_state: fnet_routes::StateV6Proxy,
    pub(crate) v4_main_route_table: fnet_routes_admin::RouteTableV4Proxy,
    pub(crate) v6_main_route_table: fnet_routes_admin::RouteTableV6Proxy,
    pub(crate) v4_route_table_provider: fnet_routes_admin::RouteTableProviderV4Proxy,
    pub(crate) v6_route_table_provider: fnet_routes_admin::RouteTableProviderV6Proxy,
    pub(crate) v4_rule_table: fnet_routes_admin::RuleTableV4Proxy,
    pub(crate) v6_rule_table: fnet_routes_admin::RuleTableV6Proxy,
    pub(crate) ndp_option_watcher_provider: fnet_ndp::RouterAdvertisementOptionWatcherProviderProxy,
    pub(crate) interfaces_handler: H,
    pub(crate) route_clients: ClientTable<NetlinkRoute, S>,
    pub(crate) async_work_receiver:
        futures::channel::mpsc::UnboundedReceiver<AsyncWorkItem<NetlinkRouteNotifiedGroup>>,
    pub(crate) unified_request_stream: mpsc::Receiver<UnifiedRequest<S>>,
    pub(crate) feature_flags: FeatureFlags,
}

/// The types that implement this trait ([`Optional`] and [`Required`]) are used to signify whether
/// a given [`EventLoopComponent`] can be omitted with a given [`EventLoopSpec`] configuration.
pub(crate) trait EventLoopOptionality: std::fmt::Debug + Copy {}
#[cfg(test)]
impl EventLoopOptionality for Optional {}
impl EventLoopOptionality for Required {}

/// If used to fill the `Absence` type parameter on an [`EventLoopComponent`],
/// the component can be omitted at run time for testing purposes.
#[cfg(test)]
#[derive(Copy, Clone, Debug)]
pub(crate) struct Optional;

/// An uninhabited type used to fill the `Absence` generic on an [`EventLoopComponent`] to indicate
/// the component must be present at run time. This is the only implementor of
/// [`EventLoopOptionality`] when `cfg(not(test))`.
#[derive(Copy, Clone, Debug)]
pub(crate) enum Required {}

/// A component of the netlink event loop that can be either required or optional depending on
/// the [`EventLoopSpec`] the event loop is configured with at compile time.
///
/// The event loop implementation methods on [`EventLoopInputs`] and [`EventLoopState`]
/// unwrap this only if they actually need the contents, so that tests can be run with only a
/// subset of required functionality.
pub(crate) enum EventLoopComponent<T, Absence: EventLoopOptionality> {
    Present(T),
    /// Never constructed outside of tests. This variant is uninstantiable when `Absence` is
    /// [`Required`], as [`Required`] is itself uninstantiable.
    #[cfg_attr(not(test), allow(dead_code))]
    Absent(Absence),
}

impl<T, E: EventLoopOptionality> EventLoopComponent<T, E> {
    fn get(self) -> T {
        match self {
            EventLoopComponent::Present(t) => t,
            EventLoopComponent::Absent(_) => panic!("must be present"),
        }
    }

    fn get_mut(&mut self) -> &mut T {
        match self {
            EventLoopComponent::Present(t) => t,
            EventLoopComponent::Absent(_) => panic!("must be present"),
        }
    }

    fn get_ref(&self) -> &T {
        match self {
            EventLoopComponent::Present(t) => t,
            EventLoopComponent::Absent(_) => panic!("must be present"),
        }
    }

    fn present(&self) -> Option<&T> {
        match self {
            EventLoopComponent::Present(t) => Some(t),
            EventLoopComponent::Absent(_) => None,
        }
    }

    fn present_mut(&mut self) -> Option<&mut T> {
        match self {
            EventLoopComponent::Present(t) => Some(t),
            EventLoopComponent::Absent(_) => None,
        }
    }
}

pub(crate) trait EventLoopSpec {
    type InterfacesProxy: EventLoopOptionality;
    type InterfacesStateProxy: EventLoopOptionality;
    type V4RoutesState: EventLoopOptionality;
    type V6RoutesState: EventLoopOptionality;
    type V4RoutesSetProvider: EventLoopOptionality;
    type V6RoutesSetProvider: EventLoopOptionality;
    type V4RouteTableProvider: EventLoopOptionality;
    type V6RouteTableProvider: EventLoopOptionality;
    type InterfacesHandler: EventLoopOptionality;
    type RouteClients: EventLoopOptionality;

    type RoutesV4Worker: EventLoopOptionality;
    type RoutesV6Worker: EventLoopOptionality;
    type InterfacesWorker: EventLoopOptionality;
    type RuleV4Worker: EventLoopOptionality;
    type RuleV6Worker: EventLoopOptionality;
    type NduseroptWorker: EventLoopOptionality;
}

pub(crate) struct IncludedWorkers<E: EventLoopSpec> {
    pub(crate) routes_v4: EventLoopComponent<(), E::RoutesV4Worker>,
    pub(crate) routes_v6: EventLoopComponent<(), E::RoutesV6Worker>,
    pub(crate) interfaces: EventLoopComponent<(), E::InterfacesWorker>,
    pub(crate) rules_v4: EventLoopComponent<(), E::RuleV4Worker>,
    pub(crate) rules_v6: EventLoopComponent<(), E::RuleV6Worker>,
    pub(crate) nduseropt: EventLoopComponent<(), E::NduseroptWorker>,
}

enum AllWorkers {}
impl EventLoopSpec for AllWorkers {
    type InterfacesProxy = Required;
    type InterfacesStateProxy = Required;
    type V4RoutesState = Required;
    type V6RoutesState = Required;
    type V4RoutesSetProvider = Required;
    type V6RoutesSetProvider = Required;
    type V4RouteTableProvider = Required;
    type V6RouteTableProvider = Required;
    type InterfacesHandler = Required;
    type RouteClients = Required;

    type RoutesV4Worker = Required;
    type RoutesV6Worker = Required;
    type InterfacesWorker = Required;
    type RuleV4Worker = Required;
    type RuleV6Worker = Required;
    type NduseroptWorker = Required;
}

pub(crate) struct EventLoopInputs<
    H,
    S: crate::messaging::Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>,
    E: EventLoopSpec,
> {
    pub(crate) interfaces_proxy: EventLoopComponent<fnet_root::InterfacesProxy, E::InterfacesProxy>,
    pub(crate) interfaces_state_proxy:
        EventLoopComponent<fnet_interfaces::StateProxy, E::InterfacesStateProxy>,
    pub(crate) v4_routes_state: EventLoopComponent<fnet_routes::StateV4Proxy, E::V4RoutesState>,
    pub(crate) v6_routes_state: EventLoopComponent<fnet_routes::StateV6Proxy, E::V6RoutesState>,
    pub(crate) v4_main_route_table:
        EventLoopComponent<fnet_routes_admin::RouteTableV4Proxy, E::V4RoutesSetProvider>,
    pub(crate) v6_main_route_table:
        EventLoopComponent<fnet_routes_admin::RouteTableV6Proxy, E::V6RoutesSetProvider>,
    pub(crate) v4_route_table_provider:
        EventLoopComponent<fnet_routes_admin::RouteTableProviderV4Proxy, E::V4RouteTableProvider>,
    pub(crate) v6_route_table_provider:
        EventLoopComponent<fnet_routes_admin::RouteTableProviderV6Proxy, E::V6RouteTableProvider>,
    pub(crate) v4_rule_table:
        EventLoopComponent<fnet_routes_admin::RuleTableV4Proxy, E::RuleV4Worker>,
    pub(crate) v6_rule_table:
        EventLoopComponent<fnet_routes_admin::RuleTableV6Proxy, E::RuleV6Worker>,
    pub(crate) ndp_option_watcher_provider: EventLoopComponent<
        fnet_ndp::RouterAdvertisementOptionWatcherProviderProxy,
        E::NduseroptWorker,
    >,
    pub(crate) interfaces_handler: EventLoopComponent<H, E::InterfacesHandler>,
    pub(crate) route_clients: EventLoopComponent<ClientTable<NetlinkRoute, S>, E::RouteClients>,
    pub(crate) async_work_receiver:
        futures::channel::mpsc::UnboundedReceiver<AsyncWorkItem<NetlinkRouteNotifiedGroup>>,

    pub(crate) unified_request_stream: mpsc::Receiver<UnifiedRequest<S>>,

    pub(crate) feature_flags: FeatureFlags,
}

impl<
        H: interfaces::InterfacesHandler,
        S: crate::messaging::Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>,
        E: EventLoopSpec,
    > EventLoopInputs<H, S, E>
{
    /// Creates routes and interface hanging get watchers and connects to route and
    /// interface administration protocols so that requests can be serviced by the returned
    /// `EventLoopState`.
    pub(crate) async fn initialize(
        self,
        included_workers: IncludedWorkers<E>,
    ) -> Result<EventLoopState<H, S, E>, Error> {
        let Self {
            interfaces_proxy,
            interfaces_state_proxy,
            v4_routes_state,
            v6_routes_state,
            v4_main_route_table,
            v6_main_route_table,
            v4_route_table_provider,
            v6_route_table_provider,
            v4_rule_table,
            v6_rule_table,
            ndp_option_watcher_provider,
            interfaces_handler,
            route_clients,
            async_work_receiver,
            unified_request_stream,
            feature_flags,
        } = self;
        let (routes_v4_result, routes_v6_result, interfaces_result) = futures::join!(
            async {
                match included_workers.routes_v4 {
                    EventLoopComponent::Present(()) => {
                        let (worker, map, stream) = routes::RoutesWorker::<Ipv4>::create(
                            v4_main_route_table.get_ref(),
                            v4_routes_state.get_ref(),
                            v4_route_table_provider.get(),
                        )
                        .await
                        .context("create v4 routes worker")?;
                        Ok::<_, Error>((
                            EventLoopComponent::Present(worker),
                            EventLoopComponent::Present(map),
                            stream.left_stream(),
                        ))
                    }
                    EventLoopComponent::Absent(omitted) => Ok((
                        EventLoopComponent::Absent(omitted),
                        EventLoopComponent::Absent(omitted),
                        futures::stream::pending().right_stream(),
                    )),
                }
            },
            async {
                match included_workers.routes_v6 {
                    EventLoopComponent::Present(()) => {
                        let (worker, map, stream) = routes::RoutesWorker::<Ipv6>::create(
                            v6_main_route_table.get_ref(),
                            v6_routes_state.get_ref(),
                            v6_route_table_provider.get(),
                        )
                        .await
                        .context("create v6 routes worker")?;
                        Ok::<_, Error>((
                            EventLoopComponent::Present(worker),
                            EventLoopComponent::Present(map),
                            stream.left_stream(),
                        ))
                    }
                    EventLoopComponent::Absent(omitted) => Ok((
                        EventLoopComponent::Absent(omitted),
                        EventLoopComponent::Absent(omitted),
                        futures::stream::pending().right_stream(),
                    )),
                }
            },
            async {
                match included_workers.interfaces {
                    EventLoopComponent::Present(()) => {
                        let (worker, stream) = interfaces::InterfacesWorkerState::create(
                            interfaces_handler.get(),
                            route_clients.get_ref().clone(),
                            interfaces_proxy.get_ref().clone(),
                            interfaces_state_proxy.get(),
                        )
                        .await
                        .context("create interfaces worker")?;
                        Ok::<_, Error>((EventLoopComponent::Present(worker), stream.left_stream()))
                    }
                    EventLoopComponent::Absent(omitted) => Ok((
                        EventLoopComponent::Absent(omitted),
                        futures::stream::pending().right_stream(),
                    )),
                }
            },
        );

        let (routes_v4_worker, mut v4_route_table_map, v4_route_event_stream) =
            routes_v4_result.context("create v4 routes worker")?;
        let (routes_v6_worker, mut v6_route_table_map, v6_route_event_stream) =
            routes_v6_result.context("create v6 routes worker")?;
        let (interfaces_worker, if_event_stream) =
            interfaces_result.context("create interfaces worker")?;
        let rules_v4_worker = match included_workers.rules_v4 {
            EventLoopComponent::Present(()) => {
                let worker = rules::RulesWorker::<Ipv4>::create(
                    v4_rule_table.get_ref(),
                    v4_route_table_map.get_mut(),
                )
                .await;
                EventLoopComponent::Present(worker)
            }
            EventLoopComponent::Absent(omitted) => EventLoopComponent::Absent(omitted),
        };
        let rules_v6_worker = match included_workers.rules_v6 {
            EventLoopComponent::Present(()) => {
                let worker = rules::RulesWorker::<Ipv6>::create(
                    v6_rule_table.get_ref(),
                    v6_route_table_map.get_mut(),
                )
                .await;
                EventLoopComponent::Present(worker)
            }
            EventLoopComponent::Absent(omitted) => EventLoopComponent::Absent(omitted),
        };
        let nduseropt_worker = match included_workers.nduseropt {
            EventLoopComponent::Present(()) => EventLoopComponent::Present(
                crate::nduseropt::NduseroptWorker::new(ndp_option_watcher_provider.get()),
            ),
            EventLoopComponent::Absent(omitted) => EventLoopComponent::Absent(omitted),
        };

        let unified_event_stream = futures::stream_select!(
            v4_route_event_stream
                .map(|res| {
                    res.map(UnifiedEvent::RoutesV4Event)
                        .map_err(|e| Error::new(EventStreamError::RoutesV4(e)))
                })
                .chain(futures::stream::once(futures::future::ready(Err(Error::new(
                    EventStreamEnded::RoutesV4,
                )))))
                .fuse(),
            v6_route_event_stream
                .map(|res| {
                    res.map(UnifiedEvent::RoutesV6Event)
                        .map_err(|e| Error::new(EventStreamError::RoutesV6(e)))
                })
                .chain(futures::stream::once(futures::future::ready(Err(Error::new(
                    EventStreamEnded::RoutesV6,
                )))))
                .fuse(),
            if_event_stream
                .map(|res| {
                    res.map(UnifiedEvent::InterfacesEvent)
                        .map_err(|e| Error::new(EventStreamError::Interfaces(e)))
                })
                .chain(futures::stream::once(futures::future::ready(Err(Error::new(
                    EventStreamEnded::Interfaces,
                )))))
                .fuse(),
        )
        .boxed()
        .fuse();

        Ok(EventLoopState {
            routes_v4_worker,
            routes_v6_worker,
            interfaces_worker,
            rules_v4_worker,
            rules_v6_worker,
            nduseropt_worker,
            unified_pending_request: None,
            unified_event_stream,
            route_clients,
            interfaces_proxy,
            v4_route_table_map,
            v6_route_table_map,
            async_work_receiver,
            unified_request_stream,
            feature_flags,
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum EventStreamEnded {
    #[error("routes v4 event stream ended")]
    RoutesV4,
    #[error("routes v6 event stream ended")]
    RoutesV6,
    #[error("interfaces event stream ended")]
    Interfaces,
}

#[derive(thiserror::Error, Debug)]
pub(crate) enum EventStreamError {
    #[error("error in routes v4 event stream: {0}")]
    RoutesV4(fnet_routes_ext::WatchError),
    #[error("error in routes v6 event stream: {0}")]
    RoutesV6(fnet_routes_ext::WatchError),
    #[error("error in interfaces event stream: {0}")]
    Interfaces(fidl::Error),
}

/// All of the state tracked by the netlink event loop while it is in operation.
/// Runs routes and interface hanging get watchers and connects to route and
/// interface administration protocols in order to single-threadedly service
/// incoming `UnifiedRequest`s.
pub(crate) struct EventLoopState<
    H: interfaces::InterfacesHandler,
    S: Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>,
    E: EventLoopSpec,
> {
    routes_v4_worker: EventLoopComponent<routes::RoutesWorker<Ipv4>, E::RoutesV4Worker>,
    routes_v6_worker: EventLoopComponent<routes::RoutesWorker<Ipv6>, E::RoutesV6Worker>,
    interfaces_worker:
        EventLoopComponent<interfaces::InterfacesWorkerState<H, S>, E::InterfacesWorker>,
    rules_v4_worker: EventLoopComponent<rules::RulesWorker<Ipv4>, E::RuleV4Worker>,
    rules_v6_worker: EventLoopComponent<rules::RulesWorker<Ipv6>, E::RuleV6Worker>,
    nduseropt_worker: EventLoopComponent<crate::nduseropt::NduseroptWorker, E::NduseroptWorker>,

    route_clients: EventLoopComponent<ClientTable<NetlinkRoute, S>, E::RouteClients>,
    interfaces_proxy: EventLoopComponent<fnet_root::InterfacesProxy, E::InterfacesProxy>,
    async_work_receiver:
        futures::channel::mpsc::UnboundedReceiver<AsyncWorkItem<NetlinkRouteNotifiedGroup>>,

    v4_route_table_map: EventLoopComponent<route_tables::RouteTableMap<Ipv4>, E::RoutesV4Worker>,
    v6_route_table_map: EventLoopComponent<route_tables::RouteTableMap<Ipv6>, E::RoutesV6Worker>,
    unified_pending_request: Option<UnifiedPendingRequest<S>>,
    unified_request_stream: mpsc::Receiver<UnifiedRequest<S>>,
    unified_event_stream: futures::stream::Fuse<BoxStream<'static, Result<UnifiedEvent, Error>>>,

    feature_flags: FeatureFlags,
}

impl<
        H: interfaces::InterfacesHandler,
        S: Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>,
        E: EventLoopSpec,
    > EventLoopState<H, S, E>
{
    #[cfg(test)]
    pub(crate) fn route_table_state<
        I: fnet_routes_ext::FidlRouteIpExt + fnet_routes_ext::admin::FidlRouteAdminIpExt,
    >(
        &mut self,
    ) -> (&mut routes::RoutesWorker<I>, &mut crate::route_tables::RouteTableMap<I>) {
        I::map_ip_out(
            self,
            |me| {
                let EventLoopState { routes_v4_worker, v4_route_table_map, .. } = me;
                (routes_v4_worker.get_mut(), v4_route_table_map.get_mut())
            },
            |me| {
                let EventLoopState { routes_v6_worker, v6_route_table_map, .. } = me;
                (routes_v6_worker.get_mut(), v6_route_table_map.get_mut())
            },
        )
    }

    pub(crate) async fn run(mut self) -> Result<Never, Error> {
        loop {
            self.run_one_step().await?;
        }
    }

    async fn run_one_step(&mut self) -> Result<(), Error> {
        let Self {
            routes_v4_worker,
            routes_v6_worker,
            interfaces_worker,
            rules_v4_worker,
            rules_v6_worker,
            nduseropt_worker,
            unified_pending_request,
            unified_request_stream,
            unified_event_stream,
            route_clients,
            interfaces_proxy,
            async_work_receiver,
            v4_route_table_map,
            v6_route_table_map,
            feature_flags,
        } = self;

        let mut unified_request_stream = unified_request_stream.chain(futures::stream::pending());
        let request_fut = match unified_pending_request {
            None => unified_request_stream.next().left_future(),
            Some(unified_pending_request) => {
                log_debug!(
                    "not awaiting on request stream because of pending request: {:?}",
                    unified_pending_request,
                );
                futures::future::pending().right_future()
            }
        }
        .fuse();
        let mut request_fut = pin!(request_fut);

        let cleanup = futures::select_biased! {
            nduseropt_message = nduseropt_worker.present_mut()
                    .map(|worker| worker.select_next_message().fuse().left_future())
                    .unwrap_or_else(|| futures::future::pending().right_future()) => {

                if feature_flags.use_ndp_watcher_instead_of_nduseropt_stub {
                    route_clients.get_ref().send_message_to_group(
                        nduseropt_message,
                        ModernGroup(rtnetlink_groups_RTNLGRP_ND_USEROPT),
                    );
                }
                Cleanup::None
            }
            async_work = async_work_receiver.select_next_some() => {
                match async_work {
                    AsyncWorkItem::OnJoinMulticastGroup(
                            NetlinkRouteNotifiedGroup::Nduseropt, sender) => {
                        nduseropt_worker.get_mut().increment_clients_count().await;
                        sender.send(());
                    }
                    AsyncWorkItem::OnLeaveMulticastGroup(groups) => {
                        for group in groups.into_iter() {
                            match group {
                                NetlinkRouteNotifiedGroup::Nduseropt => {
                                    // NB: `groups` is a set, so each group only
                                    // occurs once.
                                    nduseropt_worker.get_mut().decrement_clients_count();
                                }
                            }
                        }
                    }
                    AsyncWorkItem::OnSetMulticastGroups { joined, left, complete } => {
                        match joined {
                            Some(groups) => {
                                for group in groups.into_iter() {
                                    match group {
                                        NetlinkRouteNotifiedGroup::Nduseropt => {
                                            // NB: `groups` is a set, so each group only
                                            // occurs once.
                                            nduseropt_worker.get_mut().increment_clients_count()
                                            .await;
                                        }
                                    }
                                }
                            }
                            None => (),
                        }
                        match left {
                            Some(groups) => {
                                for group in groups.into_iter() {
                                    match group {
                                        NetlinkRouteNotifiedGroup::Nduseropt => {
                                            // NB: `groups` is a set, so each group only
                                            // occurs once.
                                            nduseropt_worker.get_mut().decrement_clients_count();
                                        }
                                    }
                                }
                            }
                            None => (),
                        }
                        if let Some(complete) = complete {
                            complete.send(());
                        }
                    }
                }
                Cleanup::None
            }
            event = unified_event_stream.next() => {
                match event
                    .expect("event stream cannot end without error")? {
                    UnifiedEvent::RoutesV4Event(event) => {
                        routes_v4_worker.get_mut()
                        .handle_route_watcher_event(
                            v4_route_table_map.get_mut(),
                            route_clients.get_ref(),
                            event)
                        .map_err(Error::new)
                        .map(|opt| opt.map_or(Cleanup::None,
                            |table| Cleanup::RouteTable(CleanUpRouteTable {
                                table,
                                worker: WorkerTriggeringCleanup::RoutesV4
                            }),
                        ))
                        .context("handle v4 routes event")?
                    },
                    UnifiedEvent::RoutesV6Event(event) => {
                        routes_v6_worker.get_mut()
                        .handle_route_watcher_event(
                            v6_route_table_map.get_mut(),
                            route_clients.get_ref(),
                            event)
                        .map_err(Error::new)
                        .map(|opt| opt.map_or(Cleanup::None,
                            |table| Cleanup::RouteTable(CleanUpRouteTable {
                                table,
                                worker: WorkerTriggeringCleanup::RoutesV6
                            }),
                        ))
                        .context("handle v6 routes event")?
                    },
                    UnifiedEvent::InterfacesEvent(event) => interfaces_worker.get_mut()
                        .handle_interface_watcher_event(event, feature_flags).await
                        .map_err(Error::new)
                        .map(|()| Cleanup::None)
                        .context("handle interfaces event")?,
                }
            }
            request = request_fut => {
                assert_matches!(
                    unified_pending_request,
                    None,
                    "should not already have pending request if handling a new request"
                );

                match request.expect("request stream cannot end") {
                    UnifiedRequest::InterfacesRequest(request) => {
                        let request = interfaces_worker.get_mut()
                            .handle_request(request).await;
                        *unified_pending_request = request.map(UnifiedPendingRequest::Interfaces);
                        Cleanup::None
                    }
                    UnifiedRequest::RoutesV4Request(request) => {
                        let request = routes_v4_worker.get_mut()
                            .handle_request(
                                v4_route_table_map.get_mut(),
                                interfaces_proxy.get_ref(),
                                request,
                            ).await;
                        *unified_pending_request = request.map(UnifiedPendingRequest::RoutesV4);
                        Cleanup::None
                    }
                    UnifiedRequest::RoutesV6Request(request) => {
                        let request = routes_v6_worker.get_mut()
                            .handle_request(
                                v6_route_table_map.get_mut(),
                                interfaces_proxy.get_ref(),
                                request,
                            ).await;
                        *unified_pending_request = request.map(UnifiedPendingRequest::RoutesV6);
                        Cleanup::None
                    }
                    UnifiedRequest::RuleV4Request(request, completer) => {
                        let result = rules_v4_worker.get_mut()
                            .handle_request(
                                request,
                                v4_route_table_map.get_mut(),
                                &interfaces_worker.get_ref().interface_properties
                            ).await;
                        completer.send(result.map(|_| ()))
                            .expect("receiving end of completer should not be dropped");
                        match result {
                            Ok(cleanup) => {
                                cleanup.map_or(
                                    Cleanup::None,
                                    |table| Cleanup::RouteTable(
                                        CleanUpRouteTable {
                                            table,
                                            worker: WorkerTriggeringCleanup::RulesV4,
                                        }
                                    ),
                                )
                            }
                            Err(_) => Cleanup::None,
                        }
                    }
                    UnifiedRequest::RuleV6Request(request, completer) => {
                        let result = rules_v6_worker.get_mut()
                            .handle_request(
                                request,
                                v6_route_table_map.get_mut(),
                                &interfaces_worker.get_ref().interface_properties
                            ).await;
                        completer.send(result.map(|_| ()))
                            .expect("receiving end of completer should not be dropped");
                        match result {
                            Ok(cleanup) => {
                                cleanup.map_or(
                                    Cleanup::None,
                                    |table| Cleanup::RouteTable(
                                        CleanUpRouteTable {
                                            table,
                                            worker: WorkerTriggeringCleanup::RulesV6,
                                        }
                                    ),
                                )
                            }
                            Err(_) => Cleanup::None,
                        }
                    }
                }
            }
        };

        match cleanup {
            Cleanup::None => (),
            Cleanup::RouteTable(cleanup) => {
                Self::check_and_clean_up_unused_route_table(
                    cleanup,
                    routes_v4_worker,
                    routes_v6_worker,
                    rules_v4_worker,
                    rules_v6_worker,
                    v4_route_table_map,
                    v6_route_table_map,
                )
                .await
            }
        }

        let pending_request = unified_pending_request.take();
        *unified_pending_request = pending_request.and_then(|pending| match pending {
            UnifiedPendingRequest::RoutesV4(pending_request) => routes_v4_worker
                .get_mut()
                .handle_pending_request(v4_route_table_map.get_mut(), pending_request)
                .map(UnifiedPendingRequest::RoutesV4),
            UnifiedPendingRequest::RoutesV6(pending_request) => routes_v6_worker
                .get_mut()
                .handle_pending_request(v6_route_table_map.get_mut(), pending_request)
                .map(UnifiedPendingRequest::RoutesV6),
            UnifiedPendingRequest::Interfaces(pending_request) => interfaces_worker
                .get_mut()
                .handle_pending_request(pending_request)
                .map(UnifiedPendingRequest::Interfaces),
        });

        Ok(())
    }

    /// Given a route table, checks if the route table needs to be cleaned up.
    /// If so, removes the route table from the relevant [`crate::route_tables::RouteTableMap`].
    async fn check_and_clean_up_unused_route_table(
        cleanup: CleanUpRouteTable,
        routes_v4_worker: &mut EventLoopComponent<routes::RoutesWorker<Ipv4>, E::RoutesV4Worker>,
        routes_v6_worker: &mut EventLoopComponent<routes::RoutesWorker<Ipv6>, E::RoutesV6Worker>,
        rules_v4_worker: &mut EventLoopComponent<rules::RulesWorker<Ipv4>, E::RuleV4Worker>,
        rules_v6_worker: &mut EventLoopComponent<rules::RulesWorker<Ipv6>, E::RuleV6Worker>,
        v4_route_table_map: &mut EventLoopComponent<
            crate::route_tables::RouteTableMap<Ipv4>,
            E::RoutesV4Worker,
        >,
        v6_route_table_map: &mut EventLoopComponent<
            crate::route_tables::RouteTableMap<Ipv6>,
            E::RoutesV6Worker,
        >,
    ) {
        let CleanUpRouteTable {
            table: table @ route_tables::TableNeedsCleanup(table_id, table_index),
            worker,
        } = cleanup;
        match worker {
            WorkerTriggeringCleanup::RoutesV4 => {
                // Check to see if any rules reference this.
                let referenced = rules_v4_worker
                    .present()
                    .map_or(false, |worker| worker.any_rules_reference_table(table_index));

                // The routes worker only indicates cleanup is needed once the last route is
                // removed from a table, so we don't need to check if any routes reference
                // the table.
                debug_assert!(!routes_v4_worker.get_ref().any_routes_reference_table(table));

                if !referenced {
                    assert_matches!(
                        v4_route_table_map.get_mut().remove_table_by_fidl_id(table_id),
                        Some(_)
                    );
                }
            }
            WorkerTriggeringCleanup::RoutesV6 => {
                // Check to see if any rules reference this.
                let referenced = rules_v6_worker
                    .present()
                    .map_or(false, |worker| worker.any_rules_reference_table(table_index));

                // The routes worker only indicates cleanup is needed once the last route is
                // removed from a table, so we don't need to check if any routes reference
                // the table.
                debug_assert!(!routes_v6_worker.get_ref().any_routes_reference_table(table));

                if !referenced {
                    assert_matches!(
                        v6_route_table_map.get_mut().remove_table_by_fidl_id(table_id),
                        Some(_)
                    );
                }
            }
            WorkerTriggeringCleanup::RulesV4 => {
                // Check to see if any routes reference this.
                let referenced_routes = routes_v4_worker
                    .present()
                    .map_or(false, |worker| worker.any_routes_reference_table(table));
                // Check to see if any rules reference this.
                let referenced_rules =
                    rules_v4_worker.get_ref().any_rules_reference_table(table_index);
                if !referenced_routes && !referenced_rules {
                    assert_matches!(
                        v4_route_table_map.get_mut().remove_table_by_fidl_id(table_id),
                        Some(_)
                    );
                }
            }
            WorkerTriggeringCleanup::RulesV6 => {
                // Check to see if any routes reference this.
                let referenced_routes = routes_v6_worker
                    .present()
                    .map_or(false, |worker| worker.any_routes_reference_table(table));
                // Check to see if any rules reference this.
                let referenced_rules =
                    rules_v6_worker.get_ref().any_rules_reference_table(table_index);
                if !referenced_routes && !referenced_rules {
                    assert_matches!(
                        v6_route_table_map.get_mut().remove_table_by_fidl_id(table_id),
                        Some(_)
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn run_one_step_in_tests(&mut self) -> Result<(), Error> {
        self.run_one_step().await
    }
}

impl<
        H: interfaces::InterfacesHandler,
        S: crate::messaging::Sender<<NetlinkRoute as ProtocolFamily>::InnerMessage>,
    > EventLoop<H, S>
{
    pub(crate) async fn run(
        self,
        on_initialized: Option<oneshot::Sender<()>>,
    ) -> Result<Never, Error> {
        let Self {
            interfaces_proxy,
            interfaces_state_proxy,
            v4_routes_state,
            v6_routes_state,
            v4_main_route_table,
            v6_main_route_table,
            v4_route_table_provider,
            v6_route_table_provider,
            v4_rule_table,
            v6_rule_table,
            ndp_option_watcher_provider,
            interfaces_handler,
            route_clients,
            async_work_receiver,
            unified_request_stream,
            feature_flags,
        } = self;

        let state = EventLoopInputs::<_, _, AllWorkers> {
            interfaces_proxy: EventLoopComponent::Present(interfaces_proxy),
            interfaces_state_proxy: EventLoopComponent::Present(interfaces_state_proxy),
            v4_routes_state: EventLoopComponent::Present(v4_routes_state),
            v6_routes_state: EventLoopComponent::Present(v6_routes_state),
            v4_main_route_table: EventLoopComponent::Present(v4_main_route_table),
            v6_main_route_table: EventLoopComponent::Present(v6_main_route_table),
            v4_route_table_provider: EventLoopComponent::Present(v4_route_table_provider),
            v6_route_table_provider: EventLoopComponent::Present(v6_route_table_provider),
            v4_rule_table: EventLoopComponent::Present(v4_rule_table),
            v6_rule_table: EventLoopComponent::Present(v6_rule_table),
            ndp_option_watcher_provider: EventLoopComponent::Present(ndp_option_watcher_provider),
            interfaces_handler: EventLoopComponent::Present(interfaces_handler),
            route_clients: EventLoopComponent::Present(route_clients),
            async_work_receiver,
            unified_request_stream,
            feature_flags,
        }
        .initialize(IncludedWorkers {
            routes_v4: EventLoopComponent::Present(()),
            routes_v6: EventLoopComponent::Present(()),
            interfaces: EventLoopComponent::Present(()),
            rules_v4: EventLoopComponent::Present(()),
            rules_v6: EventLoopComponent::Present(()),
            nduseropt: EventLoopComponent::Present(()),
        })
        .await?;

        log_info!("routes and interfaces workers initialized, beginning execution");
        if let Some(on_initialized) = on_initialized {
            on_initialized
                .send(())
                .expect("caller should not have dropped `on_initialized` receiver");
        }

        state.run().await
    }
}

pub(crate) enum WorkerTriggeringCleanup {
    RulesV4,
    RulesV6,
    RoutesV4,
    RoutesV6,
}

pub(crate) enum Cleanup {
    None,
    RouteTable(CleanUpRouteTable),
}

pub(crate) struct CleanUpRouteTable {
    table: route_tables::TableNeedsCleanup,
    worker: WorkerTriggeringCleanup,
}
