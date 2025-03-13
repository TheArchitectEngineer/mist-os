// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod ap;
pub mod client;

use crate::{MlmeEventStream, MlmeStream, Station};
use anyhow::format_err;
use fidl::endpoints::ServerEnd;
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::future::FutureObj;
use futures::prelude::*;
use futures::select;
use futures::stream::FuturesUnordered;
use log::{error, info, warn};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use wlan_common::timer::{self, ScheduledEvent};
use {
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_mlme as fidl_mlme,
    fidl_fuchsia_wlan_sme as fidl_sme, fuchsia_inspect_auto_persist as auto_persist,
};

pub type ClientSmeServer = mpsc::UnboundedSender<client::Endpoint>;
pub type ApSmeServer = mpsc::UnboundedSender<ap::Endpoint>;

#[derive(Clone)]
pub enum SmeServer {
    Client(ClientSmeServer),
    Ap(ApSmeServer),
}

async fn serve_generic_sme(
    mut generic_sme_request_stream: <fidl_sme::GenericSmeMarker as fidl::endpoints::ProtocolMarker>::RequestStream,
    mlme_sink: crate::MlmeSink,
    mut sme_server_sender: SmeServer,
    mut telemetry_server_sender: Option<
        mpsc::UnboundedSender<fidl::endpoints::ServerEnd<fidl_sme::TelemetryMarker>>,
    >,
    mut feature_support_server_sender: mpsc::UnboundedSender<
        fidl::endpoints::ServerEnd<fidl_sme::FeatureSupportMarker>,
    >,
) -> Result<(), anyhow::Error> {
    loop {
        match generic_sme_request_stream.next().await {
            Some(Ok(req)) => {
                let result = match req {
                    fidl_sme::GenericSmeRequest::Query { responder } => {
                        let (info_responder, info_receiver) = crate::responder::Responder::new();
                        mlme_sink.send(crate::MlmeRequest::QueryDeviceInfo(info_responder));
                        let info = info_receiver.await?;
                        responder.send(&fidl_sme::GenericSmeQuery {
                            role: info.role,
                            sta_addr: info.sta_addr,
                        })
                    }
                    fidl_sme::GenericSmeRequest::GetClientSme { sme_server, responder } => {
                        let response =
                            if let SmeServer::Client(server_sender) = &mut sme_server_sender {
                                server_sender
                                    .send(sme_server)
                                    .await
                                    .map_err(|_| zx::Status::PEER_CLOSED.into_raw())
                            } else {
                                Err(zx::Status::NOT_SUPPORTED.into_raw())
                            };
                        responder.send(response)
                    }
                    fidl_sme::GenericSmeRequest::GetApSme { sme_server, responder } => {
                        let response = if let SmeServer::Ap(server_sender) = &mut sme_server_sender
                        {
                            server_sender
                                .send(sme_server)
                                .await
                                .map_err(|_| zx::Status::PEER_CLOSED.into_raw())
                        } else {
                            Err(zx::Status::NOT_SUPPORTED.into_raw())
                        };
                        responder.send(response)
                    }
                    fidl_sme::GenericSmeRequest::GetSmeTelemetry {
                        telemetry_server,
                        responder,
                    } => {
                        let response = if let Some(server) = telemetry_server_sender.as_mut() {
                            server
                                .send(telemetry_server)
                                .await
                                .map_err(|_| zx::Status::PEER_CLOSED.into_raw())
                        } else {
                            warn!("Requested unsupported SME telemetry API");
                            Err(zx::Status::NOT_SUPPORTED.into_raw())
                        };
                        responder.send(response)
                    }
                    fidl_sme::GenericSmeRequest::GetFeatureSupport {
                        feature_support_server,
                        responder,
                    } => {
                        let response = feature_support_server_sender
                            .send(feature_support_server)
                            .await
                            .map_err(|_| zx::Status::PEER_CLOSED.into_raw());
                        responder.send(response)
                    }
                };
                if let Err(e) = result {
                    error!("Failed to respond to SME handle request: {}", e);
                }
            }
            Some(Err(e)) => {
                return Err(format_err!("Generic SME request stream failed: {}", e));
            }
            None => {
                info!("Generic SME request stream terminated. Shutting down.");
                return Ok(());
            }
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    reason = "mass allow for https://fxbug.dev/381896734"
)]
pub fn create_sme(
    cfg: crate::Config,
    mlme_event_stream: MlmeEventStream,
    device_info: &fidl_mlme::DeviceInfo,
    mac_sublayer_support: fidl_common::MacSublayerSupport,
    security_support: fidl_common::SecuritySupport,
    spectrum_management_support: fidl_common::SpectrumManagementSupport,
    inspector: fuchsia_inspect::Inspector,
    persistence_req_sender: auto_persist::PersistenceReqSender,
    generic_sme_request_stream: <fidl_sme::GenericSmeMarker as fidl::endpoints::ProtocolMarker>::RequestStream,
) -> Result<(MlmeStream, Pin<Box<impl Future<Output = Result<(), anyhow::Error>>>>), anyhow::Error>
{
    let device_info = device_info.clone();
    let inspect_node = inspector.root().create_child("usme");
    let (server, mlme_req_sink, mlme_req_stream, telemetry_sender, sme_fut) = match device_info.role
    {
        fidl_common::WlanMacRole::Client => {
            let (telemetry_endpoint_sender, telemetry_endpoint_receiver) = mpsc::unbounded();
            let (sender, receiver) = mpsc::unbounded();
            let (mlme_req_sink, mlme_req_stream, fut) = client::serve(
                cfg,
                device_info,
                security_support,
                spectrum_management_support,
                mlme_event_stream,
                receiver,
                telemetry_endpoint_receiver,
                inspector,
                inspect_node,
                persistence_req_sender,
            );
            (
                SmeServer::Client(sender),
                mlme_req_sink,
                mlme_req_stream,
                Some(telemetry_endpoint_sender),
                FutureObj::new(Box::new(fut)),
            )
        }
        fidl_common::WlanMacRole::Ap => {
            let (sender, receiver) = mpsc::unbounded();
            let (mlme_req_sink, mlme_req_stream, fut) =
                ap::serve(device_info, mac_sublayer_support, mlme_event_stream, receiver);
            (
                SmeServer::Ap(sender),
                mlme_req_sink,
                mlme_req_stream,
                None,
                FutureObj::new(Box::new(fut)),
            )
        }
        fidl_common::WlanMacRole::Mesh => {
            return Err(format_err!("Mesh mode is unsupported"));
        }
        fidl_common::WlanMacRoleUnknown!() => {
            return Err(format_err!("Unknown WlanMacRole type: {:?}", device_info.role));
        }
    };
    let (feature_support_sender, feature_support_receiver) = mpsc::unbounded();
    let feature_support_fut =
        serve_fidl(mlme_req_sink.clone(), feature_support_receiver, handle_feature_support_query)
            .map(|result| result.map(|_| ()));
    let generic_sme_fut = serve_generic_sme(
        generic_sme_request_stream,
        mlme_req_sink,
        server,
        telemetry_sender,
        feature_support_sender,
    );
    let unified_fut = async move {
        select! {
            sme_fut = sme_fut.fuse() => sme_fut,
            generic_sme_fut = generic_sme_fut.fuse() => generic_sme_fut,
            feature_support_fut = feature_support_fut.fuse() => feature_support_fut,
        }
    };
    Ok((mlme_req_stream, Box::pin(unified_fut)))
}

async fn handle_feature_support_query(
    mlme_sink: crate::MlmeSink,
    query: fidl_sme::FeatureSupportRequest,
) -> Result<(), fidl::Error> {
    match query {
        fidl_sme::FeatureSupportRequest::QueryDiscoverySupport { responder } => {
            let (mlme_responder, mlme_receiver) = crate::responder::Responder::new();
            mlme_sink.send(crate::MlmeRequest::QueryDiscoverySupport(mlme_responder));
            responder
                .send(mlme_receiver.await.as_ref().map_err(|_| zx::Status::CANCELED.into_raw()))
        }
        fidl_sme::FeatureSupportRequest::QueryMacSublayerSupport { responder } => {
            let (mlme_responder, mlme_receiver) = crate::responder::Responder::new();
            mlme_sink.send(crate::MlmeRequest::QueryMacSublayerSupport(mlme_responder));
            responder
                .send(mlme_receiver.await.as_ref().map_err(|_| zx::Status::CANCELED.into_raw()))
        }
        fidl_sme::FeatureSupportRequest::QuerySecuritySupport { responder } => {
            let (mlme_responder, mlme_receiver) = crate::responder::Responder::new();
            mlme_sink.send(crate::MlmeRequest::QuerySecuritySupport(mlme_responder));
            responder
                .send(mlme_receiver.await.as_ref().map_err(|_| zx::Status::CANCELED.into_raw()))
        }
        fidl_sme::FeatureSupportRequest::QuerySpectrumManagementSupport { responder } => {
            let (mlme_responder, mlme_receiver) = crate::responder::Responder::new();
            mlme_sink.send(crate::MlmeRequest::QuerySpectrumManagementSupport(mlme_responder));
            responder
                .send(mlme_receiver.await.as_ref().map_err(|_| zx::Status::CANCELED.into_raw()))
        }
    }
}

// The returned future successfully terminates when MLME closes the channel
async fn serve_mlme_sme<STA, TS>(
    mut event_stream: MlmeEventStream,
    station: Arc<Mutex<STA>>,
    time_stream: TS,
) -> Result<(), anyhow::Error>
where
    STA: Station,
    TS: Stream<Item = ScheduledEvent<<STA as crate::Station>::Event>> + Unpin,
{
    let mut timeout_stream = timer::make_async_timed_event_stream(time_stream).fuse();

    loop {
        select! {
            // Fuse rationale: any `none`s in the MLME stream should result in
            // bailing immediately, so we don't need to track if we've seen a
            // `None` or not and can `fuse` directly in the `select` call.
            mlme_event = event_stream.next() => match mlme_event {
                Some(mlme_event) => station.lock().on_mlme_event(mlme_event),
                None => return Ok(()),
            },
            timeout = timeout_stream.next() => match timeout {
                Some(timed_event) => station.lock().on_timeout(timed_event),
                None => return Err(format_err!("SME timer stream has ended unexpectedly")),
            },
        }
    }
}

#[allow(clippy::extra_unused_lifetimes, reason = "mass allow for https://fxbug.dev/381896734")]
async fn serve_fidl<
    'a,
    C: Clone,
    T: fidl::endpoints::ProtocolMarker,
    Fut: futures::Future<Output = Result<(), fidl::Error>>,
>(
    context: C,
    new_fidl_clients: mpsc::UnboundedReceiver<ServerEnd<T>>,
    event_handler: impl Fn(C, fidl::endpoints::Request<T>) -> Fut + Copy,
) -> Result<Infallible, anyhow::Error> {
    let mut new_fidl_clients = new_fidl_clients.fuse();
    let mut fidl_clients = FuturesUnordered::new();
    loop {
        select! {
            new_fidl_client = new_fidl_clients.next() => match new_fidl_client {
                Some(end) => fidl_clients.push(serve_fidl_endpoint(context.clone(), end, event_handler)),
                None => return Err(format_err!("New FIDL client stream unexpectedly ended")),
            },
            () = fidl_clients.select_next_some() => {},
        }
    }
}

#[allow(
    clippy::extra_unused_lifetimes,
    clippy::needless_return,
    reason = "mass allow for https://fxbug.dev/381896734"
)]
async fn serve_fidl_endpoint<
    'a,
    C: Clone,
    T: fidl::endpoints::ProtocolMarker,
    Fut: futures::Future<Output = Result<(), fidl::Error>>,
>(
    context: C,
    endpoint: ServerEnd<T>,
    event_handler: impl Fn(C, fidl::endpoints::Request<T>) -> Fut + Copy,
) {
    let stream = endpoint.into_stream();
    const MAX_CONCURRENT_REQUESTS: usize = 1000;
    let handler = &event_handler;
    let r = stream
        .try_for_each_concurrent(MAX_CONCURRENT_REQUESTS, move |request| {
            (*handler)(context.clone(), request)
        })
        .await;
    if let Err(e) = r {
        error!("Error serving FIDL: {}", e);
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils;
    use fidl::endpoints::{create_proxy, create_proxy_and_stream};
    use fuchsia_async as fasync;
    use fuchsia_inspect::Inspector;
    use futures::task::Poll;
    use std::pin::pin;
    use test_case::test_case;
    use wlan_common::assert_variant;
    use wlan_common::test_utils::fake_features::{
        fake_mac_sublayer_support, fake_security_support, fake_spectrum_management_support_empty,
    };

    #[test]
    fn create_sme_fails_startup_role_unknown() {
        let mut _exec = fasync::TestExecutor::new();
        let inspector = Inspector::default();
        let (_mlme_event_sender, mlme_event_stream) = mpsc::unbounded();
        let (persistence_req_sender, _persistence_stream) =
            test_utils::create_inspect_persistence_channel();
        let (_generic_sme_proxy, generic_sme_stream) =
            create_proxy_and_stream::<fidl_sme::GenericSmeMarker>();
        let device_info = fidl_mlme::DeviceInfo {
            role: fidl_common::WlanMacRole::unknown(),
            ..test_utils::fake_device_info([0; 6].into())
        };
        let result = create_sme(
            crate::Config::default(),
            mlme_event_stream,
            &device_info,
            fake_mac_sublayer_support(),
            fake_security_support(),
            fake_spectrum_management_support_empty(),
            inspector,
            persistence_req_sender,
            generic_sme_stream,
        );

        assert!(result.is_err());
    }

    #[test]
    fn sme_shutdown_on_generic_sme_closed() {
        let mut exec = fasync::TestExecutor::new();
        let (_mlme_event_sender, mlme_event_stream) = mpsc::unbounded();
        let inspector = Inspector::default();
        let (persistence_req_sender, _persistence_stream) =
            test_utils::create_inspect_persistence_channel();
        let (generic_sme_proxy, generic_sme_stream) =
            create_proxy_and_stream::<fidl_sme::GenericSmeMarker>();
        let (_mlme_req_stream, serve_fut) = create_sme(
            crate::Config::default(),
            mlme_event_stream,
            &test_utils::fake_device_info([0; 6].into()),
            fake_mac_sublayer_support(),
            fake_security_support(),
            fake_spectrum_management_support_empty(),
            inspector,
            persistence_req_sender,
            generic_sme_stream,
        )
        .unwrap();
        let mut serve_fut = pin!(serve_fut);
        assert_variant!(exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Also close secondary SME endpoint in the Generic SME.
        drop(generic_sme_proxy);

        // Verify SME future finished cleanly.
        assert_variant!(exec.run_until_stalled(&mut serve_fut), Poll::Ready(Ok(())));
    }

    struct GenericSmeTestHelper {
        proxy: fidl_sme::GenericSmeProxy,
        mlme_req_stream: MlmeStream,

        // These values must stay in scope or the SME will terminate, but they
        // are not relevant to Generic SME tests.
        _inspector: Inspector,
        _persistence_stream: mpsc::Receiver<String>,
        _mlme_event_sender: mpsc::UnboundedSender<crate::MlmeEvent>,
        // Executor goes last to avoid test shutdown failures.
        exec: fasync::TestExecutor,
    }

    #[allow(clippy::type_complexity, reason = "mass allow for https://fxbug.dev/381896734")]
    fn start_generic_sme_test(
        role: fidl_common::WlanMacRole,
    ) -> Result<
        (GenericSmeTestHelper, Pin<Box<impl Future<Output = Result<(), anyhow::Error>>>>),
        anyhow::Error,
    > {
        let mut exec = fasync::TestExecutor::new();
        let inspector = Inspector::default();
        let (mlme_event_sender, mlme_event_stream) = mpsc::unbounded();
        let (persistence_req_sender, persistence_stream) =
            test_utils::create_inspect_persistence_channel();
        let (generic_sme_proxy, generic_sme_stream) =
            create_proxy_and_stream::<fidl_sme::GenericSmeMarker>();
        let device_info =
            fidl_mlme::DeviceInfo { role, ..test_utils::fake_device_info([0; 6].into()) };
        let (mlme_req_stream, serve_fut) = create_sme(
            crate::Config::default(),
            mlme_event_stream,
            &device_info,
            fake_mac_sublayer_support(),
            fake_security_support(),
            fake_spectrum_management_support_empty(),
            inspector.clone(),
            persistence_req_sender,
            generic_sme_stream,
        )?;
        let mut serve_fut = Box::pin(serve_fut);
        assert_variant!(exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        Ok((
            GenericSmeTestHelper {
                proxy: generic_sme_proxy,
                mlme_req_stream,
                _inspector: inspector,
                _persistence_stream: persistence_stream,
                _mlme_event_sender: mlme_event_sender,
                exec,
            },
            serve_fut,
        ))
    }

    #[test]
    fn generic_sme_get_client() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Client).unwrap();

        let (client_proxy, client_server) = create_proxy();
        let mut client_sme_fut = helper.proxy.get_client_sme(client_server);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(
            helper.exec.run_until_stalled(&mut client_sme_fut),
            Poll::Ready(Ok(Ok(())))
        );

        let mut status_fut = client_proxy.status();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(
            helper.exec.run_until_stalled(&mut status_fut),
            Poll::Ready(Ok(fidl_sme::ClientStatusResponse::Idle(_)))
        );
    }

    #[test]
    fn generic_sme_get_ap_from_client_fails() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Client).unwrap();

        let (_ap_proxy, ap_server) = create_proxy();
        let mut client_sme_fut = helper.proxy.get_ap_sme(ap_server);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(
            helper.exec.run_until_stalled(&mut client_sme_fut),
            Poll::Ready(Ok(Err(_)))
        );
    }

    #[test]
    fn generic_sme_get_ap() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Ap).unwrap();

        let (ap_proxy, ap_server) = create_proxy();
        let mut ap_sme_fut = helper.proxy.get_ap_sme(ap_server);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(helper.exec.run_until_stalled(&mut ap_sme_fut), Poll::Ready(Ok(Ok(()))));

        let mut status_fut = ap_proxy.status();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(
            helper.exec.run_until_stalled(&mut status_fut),
            Poll::Ready(Ok(fidl_sme::ApStatusResponse { .. }))
        );
    }

    #[test]
    fn generic_sme_get_client_from_ap_fails() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Ap).unwrap();

        let (_client_proxy, client_server) = create_proxy();
        let mut client_sme_fut = helper.proxy.get_client_sme(client_server);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(
            helper.exec.run_until_stalled(&mut client_sme_fut),
            Poll::Ready(Ok(Err(_)))
        );
    }

    fn get_telemetry_proxy(
        helper: &mut GenericSmeTestHelper,
        serve_fut: &mut Pin<Box<impl Future<Output = Result<(), anyhow::Error>>>>,
    ) -> fidl_sme::TelemetryProxy {
        let (proxy, server) = create_proxy();
        let mut telemetry_fut = helper.proxy.get_sme_telemetry(server);
        assert_variant!(helper.exec.run_until_stalled(serve_fut), Poll::Pending);
        assert_variant!(helper.exec.run_until_stalled(&mut telemetry_fut), Poll::Ready(Ok(Ok(()))));
        proxy
    }

    #[test]
    fn generic_sme_query_telemetry_support_for_client() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Client).unwrap();
        let telemetry_proxy = get_telemetry_proxy(&mut helper, &mut serve_fut);

        // Forward request to MLME.
        let mut support_fut = telemetry_proxy.query_telemetry_support();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME. Use a fake error code to make the response easily verifiable.
        let support_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let support_responder = assert_variant!(support_req, crate::MlmeRequest::QueryTelemetrySupport(responder) => responder);
        support_responder.respond(Err(1337));

        // Verify that the response made it to us without alteration.
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let support_result = assert_variant!(helper.exec.run_until_stalled(&mut support_fut), Poll::Ready(Ok(support_result)) => support_result);
        assert_eq!(support_result, Err(1337));
    }

    #[test]
    fn generic_sme_get_histogram_stats_for_client() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Client).unwrap();
        let telemetry_proxy = get_telemetry_proxy(&mut helper, &mut serve_fut);

        // Forward request to MLME.
        let mut histogram_fut = telemetry_proxy.get_histogram_stats();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME. Use a fake error code to make the response easily verifiable.
        let histogram_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let histogram_responder = assert_variant!(histogram_req, crate::MlmeRequest::GetIfaceHistogramStats(responder) => responder);
        histogram_responder.respond(fidl_mlme::GetIfaceHistogramStatsResponse::ErrorStatus(1337));

        // Verify that the response made it to us without alteration.
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let histogram_result = assert_variant!(helper.exec.run_until_stalled(&mut histogram_fut), Poll::Ready(Ok(histogram_result)) => histogram_result);
        assert_eq!(histogram_result, Err(1337));
    }

    #[test]
    fn generic_sme_get_counter_stats_for_client() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Client).unwrap();
        let telemetry_proxy = get_telemetry_proxy(&mut helper, &mut serve_fut);

        // Forward request to MLME.
        let mut counter_fut = telemetry_proxy.get_counter_stats();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME. Use a fake error code to make the response easily verifiable.
        let counter_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let counter_responder = assert_variant!(counter_req, crate::MlmeRequest::GetIfaceCounterStats(responder) => responder);
        counter_responder.respond(fidl_mlme::GetIfaceCounterStatsResponse::ErrorStatus(1337));

        // Verify that the response made it to us without alteration.
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let counter_result = assert_variant!(helper.exec.run_until_stalled(&mut counter_fut), Poll::Ready(Ok(counter_result)) => counter_result);
        assert_eq!(counter_result, Err(1337));
    }

    #[test]
    fn generic_sme_get_telemetry_for_ap_fails() {
        let (mut helper, mut serve_fut) =
            start_generic_sme_test(fidl_common::WlanMacRole::Ap).unwrap();

        let (_telemetry_proxy, telemetry_server) = create_proxy();
        let mut telemetry_fut = helper.proxy.get_sme_telemetry(telemetry_server);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        assert_variant!(helper.exec.run_until_stalled(&mut telemetry_fut), Poll::Ready(Ok(Err(_))));
    }

    fn get_feature_support_proxy(
        helper: &mut GenericSmeTestHelper,
        serve_fut: &mut Pin<Box<impl Future<Output = Result<(), anyhow::Error>>>>,
    ) -> fidl_sme::FeatureSupportProxy {
        let (proxy, server) = create_proxy();
        let mut features_fut = helper.proxy.get_feature_support(server);
        assert_variant!(helper.exec.run_until_stalled(serve_fut), Poll::Pending);
        assert_variant!(helper.exec.run_until_stalled(&mut features_fut), Poll::Ready(Ok(Ok(()))));
        proxy
    }

    #[test_case(fidl_common::WlanMacRole::Client)]
    #[test_case(fidl_common::WlanMacRole::Ap)]
    fn generic_sme_discovery_support_query(mac_role: fidl_common::WlanMacRole) {
        let (mut helper, mut serve_fut) = start_generic_sme_test(mac_role).unwrap();
        let feature_support_proxy = get_feature_support_proxy(&mut helper, &mut serve_fut);

        let mut discovery_support_fut = feature_support_proxy.query_discovery_support();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME.
        let discovery_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let discovery_responder = assert_variant!(discovery_req, crate::MlmeRequest::QueryDiscoverySupport(responder) => responder);
        let expected_discovery_support = fidl_common::DiscoverySupport {
            scan_offload: fidl_common::ScanOffloadExtension {
                supported: true,
                scan_cancel_supported: false,
            },
            probe_response_offload: fidl_common::ProbeResponseOffloadExtension { supported: false },
        };
        discovery_responder.respond(expected_discovery_support);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let discovery_support = assert_variant!(helper.exec.run_until_stalled(&mut discovery_support_fut), Poll::Ready(Ok(support)) => support);
        assert_eq!(discovery_support, Ok(expected_discovery_support));
    }

    #[test_case(fidl_common::WlanMacRole::Client)]
    #[test_case(fidl_common::WlanMacRole::Ap)]
    fn generic_sme_mac_sublayer_support_query(mac_role: fidl_common::WlanMacRole) {
        let (mut helper, mut serve_fut) = start_generic_sme_test(mac_role).unwrap();
        let feature_support_proxy = get_feature_support_proxy(&mut helper, &mut serve_fut);

        let mut mac_sublayer_support_fut = feature_support_proxy.query_mac_sublayer_support();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME.
        let mac_sublayer_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let mac_sublayer_responder = assert_variant!(mac_sublayer_req, crate::MlmeRequest::QueryMacSublayerSupport(responder) => responder);
        let expected_mac_sublayer_support = fidl_common::MacSublayerSupport {
            rate_selection_offload: fidl_common::RateSelectionOffloadExtension { supported: true },
            data_plane: fidl_common::DataPlaneExtension {
                data_plane_type: fidl_common::DataPlaneType::GenericNetworkDevice,
            },
            device: fidl_common::DeviceExtension {
                is_synthetic: true,
                mac_implementation_type: fidl_common::MacImplementationType::Softmac,
                tx_status_report_supported: false,
            },
        };
        mac_sublayer_responder.respond(expected_mac_sublayer_support);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let mac_sublayer_support = assert_variant!(helper.exec.run_until_stalled(&mut mac_sublayer_support_fut), Poll::Ready(Ok(support)) => support);
        assert_eq!(mac_sublayer_support, Ok(expected_mac_sublayer_support));
    }

    #[test_case(fidl_common::WlanMacRole::Client)]
    #[test_case(fidl_common::WlanMacRole::Ap)]
    fn generic_sme_security_support_query(mac_role: fidl_common::WlanMacRole) {
        let (mut helper, mut serve_fut) = start_generic_sme_test(mac_role).unwrap();
        let feature_support_proxy = get_feature_support_proxy(&mut helper, &mut serve_fut);

        let mut security_support_fut = feature_support_proxy.query_security_support();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME.
        let security_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let security_responder = assert_variant!(security_req, crate::MlmeRequest::QuerySecuritySupport(responder) => responder);
        let expected_security_support = fidl_common::SecuritySupport {
            sae: fidl_common::SaeFeature {
                driver_handler_supported: true,
                sme_handler_supported: false,
            },
            mfp: fidl_common::MfpFeature { supported: true },
        };
        security_responder.respond(expected_security_support);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let security_support = assert_variant!(helper.exec.run_until_stalled(&mut security_support_fut), Poll::Ready(Ok(support)) => support);
        assert_eq!(security_support, Ok(expected_security_support));
    }

    #[test_case(fidl_common::WlanMacRole::Client)]
    #[test_case(fidl_common::WlanMacRole::Ap)]
    fn generic_sme_spectrum_management_query(mac_role: fidl_common::WlanMacRole) {
        let (mut helper, mut serve_fut) = start_generic_sme_test(mac_role).unwrap();
        let feature_support_proxy = get_feature_support_proxy(&mut helper, &mut serve_fut);

        let mut spectrum_support_fut = feature_support_proxy.query_spectrum_management_support();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // Mock response from MLME.
        let spectrum_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let spectrum_responder = assert_variant!(spectrum_req, crate::MlmeRequest::QuerySpectrumManagementSupport(responder) => responder);
        let expected_spectrum_support = fidl_common::SpectrumManagementSupport {
            dfs: fidl_common::DfsFeature { supported: true },
        };
        spectrum_responder.respond(expected_spectrum_support);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let spectrum_support = assert_variant!(helper.exec.run_until_stalled(&mut spectrum_support_fut), Poll::Ready(Ok(support)) => support);
        assert_eq!(spectrum_support, Ok(expected_spectrum_support));
    }

    #[test_case(fidl_common::WlanMacRole::Client)]
    #[test_case(fidl_common::WlanMacRole::Ap)]
    fn generic_sme_support_query_cancelled(mac_role: fidl_common::WlanMacRole) {
        let (mut helper, mut serve_fut) = start_generic_sme_test(mac_role).unwrap();
        let feature_support_proxy = get_feature_support_proxy(&mut helper, &mut serve_fut);

        let mut discovery_support_fut = feature_support_proxy.query_discovery_support();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        // No response from MLME, drop the responder instead. This might happen during shutdown.
        let discovery_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let discovery_responder = assert_variant!(discovery_req, crate::MlmeRequest::QueryDiscoverySupport(responder) => responder);
        drop(discovery_responder);
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let discovery_support = assert_variant!(helper.exec.run_until_stalled(&mut discovery_support_fut), Poll::Ready(Ok(support)) => support);
        assert_eq!(discovery_support, Err(zx::Status::CANCELED.into_raw()));
    }

    #[test_case(fidl_common::WlanMacRole::Client)]
    #[test_case(fidl_common::WlanMacRole::Ap)]
    fn generic_sme_query(mac_role: fidl_common::WlanMacRole) {
        let (mut helper, mut serve_fut) = start_generic_sme_test(mac_role).unwrap();

        let mut query_fut = helper.proxy.query();
        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);

        let query_req = assert_variant!(helper.exec.run_until_stalled(&mut helper.mlme_req_stream.next()), Poll::Ready(Some(req)) => req);
        let query_responder =
            assert_variant!(query_req, crate::MlmeRequest::QueryDeviceInfo(responder) => responder);
        query_responder.respond(fidl_mlme::DeviceInfo {
            role: mac_role,
            sta_addr: [2; 6],
            bands: vec![],
            softmac_hardware_capability: 0,
            qos_capable: false,
        });

        assert_variant!(helper.exec.run_until_stalled(&mut serve_fut), Poll::Pending);
        let query_result = assert_variant!(helper.exec.run_until_stalled(&mut query_fut), Poll::Ready(Ok(result)) => result);
        assert_eq!(query_result.role, mac_role);
        assert_eq!(query_result.sta_addr, [2; 6]);
    }
}
