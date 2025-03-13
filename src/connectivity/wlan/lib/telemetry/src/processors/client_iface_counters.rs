// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_wlan_stats as fidl_stats;
use fuchsia_async::TimeoutExt;
use futures::lock::Mutex;

use log::{error, warn};
use std::collections::HashMap;
use std::sync::Arc;
use windowed_stats::experimental::clock::Timed;
use windowed_stats::experimental::series::interpolation::LastSample;
use windowed_stats::experimental::series::statistic::LatchMax;
use windowed_stats::experimental::series::{SamplingProfile, TimeMatrix};
use windowed_stats::experimental::serve::{InspectSender, InspectedTimeMatrix};

// Include a timeout on stats calls so that if the driver deadlocks, telemtry doesn't get stuck.
const GET_IFACE_STATS_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

#[derive(Debug)]
enum IfaceState {
    NotAvailable,
    Created { iface_id: u16, telemetry_proxy: Option<fidl_fuchsia_wlan_sme::TelemetryProxy> },
}

pub struct ClientIfaceCountersLogger<S> {
    iface_state: Arc<Mutex<IfaceState>>,
    monitor_svc_proxy: fidl_fuchsia_wlan_device_service::DeviceMonitorProxy,
    time_series_stats: IfaceCountersTimeSeries,
    driver_specific_time_matrix_client: S,
    driver_counters_time_series: Arc<Mutex<HashMap<u16, InspectedTimeMatrix<u64>>>>,
}

impl<S: InspectSender> ClientIfaceCountersLogger<S> {
    pub fn new(
        monitor_svc_proxy: fidl_fuchsia_wlan_device_service::DeviceMonitorProxy,
        time_matrix_client: &S,
        driver_specific_time_matrix_client: S,
    ) -> Self {
        Self {
            iface_state: Arc::new(Mutex::new(IfaceState::NotAvailable)),
            monitor_svc_proxy,
            time_series_stats: IfaceCountersTimeSeries::new(time_matrix_client),
            driver_specific_time_matrix_client,
            driver_counters_time_series: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn handle_iface_created(&self, iface_id: u16) {
        let (proxy, server) = fidl::endpoints::create_proxy();
        let telemetry_proxy = match self.monitor_svc_proxy.get_sme_telemetry(iface_id, server).await
        {
            Ok(Ok(())) => {
                let inspect_counter_configs = match proxy.query_telemetry_support().await {
                    Ok(Ok(support)) => support.inspect_counter_configs,
                    Ok(Err(code)) => {
                        warn!("Failed to query telemetry support with status code {}. No driver-specific counters will be captured", code);
                        None
                    }
                    Err(e) => {
                        error!("Failed to query telemetry support with error {}. No driver-specific counters will be captured", e);
                        None
                    }
                };
                {
                    let mut driver_counters_time_series =
                        self.driver_counters_time_series.lock().await;
                    for inspect_counter_config in inspect_counter_configs.unwrap_or(vec![]) {
                        if let fidl_stats::InspectCounterConfig {
                            counter_id: Some(counter_id),
                            counter_name: Some(counter_name),
                            ..
                        } = inspect_counter_config
                        {
                            let _time_matrix_ref = driver_counters_time_series
                                .entry(counter_id)
                                .or_insert_with(|| {
                                    self.driver_specific_time_matrix_client.inspect_time_matrix(
                                        counter_name,
                                        TimeMatrix::<LatchMax<u64>, LastSample>::new(
                                            SamplingProfile::balanced(),
                                            LastSample::or(0),
                                        ),
                                    )
                                });
                        }
                    }
                }
                Some(proxy)
            }
            Ok(Err(e)) => {
                error!("Request for SME telemetry for iface {} completed with error {}. No telemetry will be captured.", iface_id, e);
                None
            }
            Err(e) => {
                error!("Failed to request SME telemetry for iface {} with error {}. No telemetry will be captured.", iface_id, e);
                None
            }
        };
        *self.iface_state.lock().await = IfaceState::Created { iface_id, telemetry_proxy }
    }

    pub async fn handle_iface_destroyed(&self, iface_id: u16) {
        let destroyed = matches!(*self.iface_state.lock().await, IfaceState::Created { iface_id: existing_iface_id, .. } if iface_id == existing_iface_id);
        if destroyed {
            *self.iface_state.lock().await = IfaceState::NotAvailable;
        }
    }

    pub async fn handle_periodic_telemetry(&self, is_connected: bool) {
        match &*self.iface_state.lock().await {
            IfaceState::NotAvailable => (),
            IfaceState::Created { telemetry_proxy, .. } => {
                if let Some(telemetry_proxy) = &telemetry_proxy {
                    match telemetry_proxy
                        .get_counter_stats()
                        .on_timeout(GET_IFACE_STATS_TIMEOUT, || {
                            warn!("Timed out waiting for counter stats");
                            Ok(Err(zx::Status::TIMED_OUT.into_raw()))
                        })
                        .await
                    {
                        Ok(Ok(stats)) => {
                            // Iface-level driver specific counters
                            if let Some(counters) = &stats.driver_specific_counters {
                                let time_series = Arc::clone(&self.driver_counters_time_series);
                                log_driver_specific_counters(&counters[..], time_series).await;
                            }
                            log_connection_counters(
                                &stats,
                                &self.time_series_stats,
                                Arc::clone(&self.driver_counters_time_series),
                            )
                            .await;
                        }
                        error => {
                            // It's normal for this call to fail while the device is not connected,
                            // so suppress the warning if that's the case.
                            if is_connected {
                                warn!("Failed to get interface stats: {:?}", error);
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn log_connection_counters(
    stats: &fidl_stats::IfaceCounterStats,
    time_series_stats: &IfaceCountersTimeSeries,
    driver_counters_time_series: Arc<Mutex<HashMap<u16, InspectedTimeMatrix<u64>>>>,
) {
    let connection_counters = match &stats.connection_counters {
        Some(counters) => counters,
        None => return,
    };

    // `connection_id` field is not used yet, but we check it anyway to
    // enforce that it must be there for us to log driver counters.
    match &connection_counters.connection_id {
        Some(_connection_id) => (),
        _ => {
            warn!("connection_id is not present, no connection counters will be logged");
            return;
        }
    }

    if let fidl_stats::ConnectionCounters {
        rx_unicast_total: Some(rx_unicast_total),
        rx_unicast_drop: Some(rx_unicast_drop),
        ..
    } = connection_counters
    {
        time_series_stats.log_rx_unicast_total(*rx_unicast_total);
        time_series_stats.log_rx_unicast_drop(*rx_unicast_drop);
    }

    if let fidl_stats::ConnectionCounters {
        tx_total: Some(tx_total), tx_drop: Some(tx_drop), ..
    } = connection_counters
    {
        time_series_stats.log_tx_total(*tx_total);
        time_series_stats.log_tx_drop(*tx_drop);
    }

    // Connection-level driver-specific counters
    if let Some(counters) = &connection_counters.driver_specific_counters {
        log_driver_specific_counters(&counters[..], driver_counters_time_series).await;
    }
}

async fn log_driver_specific_counters(
    driver_specific_counters: &[fidl_stats::UnnamedCounter],
    driver_counters_time_series: Arc<Mutex<HashMap<u16, InspectedTimeMatrix<u64>>>>,
) {
    let time_series_map = driver_counters_time_series.lock().await;
    for counter in driver_specific_counters {
        if let Some(ts) = time_series_map.get(&counter.id) {
            ts.fold_or_log_error(Timed::now(counter.count));
        }
    }
}

#[derive(Debug, Clone)]
struct IfaceCountersTimeSeries {
    rx_unicast_total: InspectedTimeMatrix<u64>,
    rx_unicast_drop: InspectedTimeMatrix<u64>,
    tx_total: InspectedTimeMatrix<u64>,
    tx_drop: InspectedTimeMatrix<u64>,
}

impl IfaceCountersTimeSeries {
    pub fn new<S: InspectSender>(client: &S) -> Self {
        let rx_unicast_total = client.inspect_time_matrix(
            "rx_unicast_total",
            TimeMatrix::<LatchMax<u64>, LastSample>::new(
                SamplingProfile::balanced(),
                LastSample::or(0),
            ),
        );
        let rx_unicast_drop = client.inspect_time_matrix(
            "rx_unicast_drop",
            TimeMatrix::<LatchMax<u64>, LastSample>::new(
                SamplingProfile::balanced(),
                LastSample::or(0),
            ),
        );
        let tx_total = client.inspect_time_matrix(
            "tx_total",
            TimeMatrix::<LatchMax<u64>, LastSample>::new(
                SamplingProfile::balanced(),
                LastSample::or(0),
            ),
        );
        let tx_drop = client.inspect_time_matrix(
            "tx_drop",
            TimeMatrix::<LatchMax<u64>, LastSample>::new(
                SamplingProfile::balanced(),
                LastSample::or(0),
            ),
        );
        Self { rx_unicast_total, rx_unicast_drop, tx_total, tx_drop }
    }

    fn log_rx_unicast_total(&self, data: u64) {
        self.rx_unicast_total.fold_or_log_error(Timed::now(data));
    }

    fn log_rx_unicast_drop(&self, data: u64) {
        self.rx_unicast_drop.fold_or_log_error(Timed::now(data));
    }

    fn log_tx_total(&self, data: u64) {
        self.tx_total.fold_or_log_error(Timed::now(data));
    }

    fn log_tx_drop(&self, data: u64) {
        self.tx_drop.fold_or_log_error(Timed::now(data));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use futures::TryStreamExt;
    use std::pin::pin;
    use std::task::Poll;
    use windowed_stats::experimental::testing::{MockTimeMatrixClient, TimeMatrixCall};
    use wlan_common::assert_variant;

    const IFACE_ID: u16 = 66;

    #[fuchsia::test]
    fn test_handle_iface_created() {
        let mut test_helper = setup_test();
        let driver_mock_matrix_client = MockTimeMatrixClient::new();
        let logger = ClientIfaceCountersLogger::new(
            test_helper.monitor_svc_proxy.clone(),
            &test_helper.mock_time_matrix_client,
            driver_mock_matrix_client.clone(),
        );

        let mut handle_iface_created_fut = pin!(logger.handle_iface_created(IFACE_ID));
        assert_eq!(
            test_helper.run_and_handle_get_sme_telemetry(&mut handle_iface_created_fut),
            Poll::Pending
        );

        let mocked_inspect_counter_configs = vec![fidl_stats::InspectCounterConfig {
            counter_id: Some(1),
            counter_name: Some("foo_counter".to_string()),
            ..Default::default()
        }];
        let telemetry_support = fidl_stats::TelemetrySupport {
            inspect_counter_configs: Some(mocked_inspect_counter_configs),
            ..Default::default()
        };
        assert_eq!(
            test_helper.run_and_respond_query_telemetry_support(
                &mut handle_iface_created_fut,
                Ok(&telemetry_support)
            ),
            Poll::Ready(())
        );

        assert_variant!(logger.iface_state.try_lock().as_deref(), Some(IfaceState::Created { .. }));
        let driver_counters_time_series = logger.driver_counters_time_series.try_lock().unwrap();
        assert_eq!(driver_counters_time_series.keys().copied().collect::<Vec<u16>>(), vec![1u16],);
    }

    #[fuchsia::test]
    fn test_handle_periodic_telemetry_connection_counters() {
        let mut test_helper = setup_test();
        let driver_mock_matrix_client = MockTimeMatrixClient::new();
        let logger = ClientIfaceCountersLogger::new(
            test_helper.monitor_svc_proxy.clone(),
            &test_helper.mock_time_matrix_client,
            driver_mock_matrix_client.clone(),
        );

        // Transition to IfaceCreated state
        handle_iface_created(&mut test_helper, &logger);

        let is_connected = true;
        let mut test_fut = pin!(logger.handle_periodic_telemetry(is_connected));
        let counter_stats = fidl_stats::IfaceCounterStats {
            connection_counters: Some(fidl_stats::ConnectionCounters {
                connection_id: Some(1),
                rx_unicast_total: Some(100),
                rx_unicast_drop: Some(5),
                rx_multicast: Some(30),
                tx_total: Some(50),
                tx_drop: Some(2),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            test_helper.run_and_respond_iface_counter_stats_req(&mut test_fut, Ok(&counter_stats)),
            Poll::Ready(())
        );

        let mut time_matrix_calls = test_helper.mock_time_matrix_client.drain_calls();
        assert_eq!(
            &time_matrix_calls.drain::<u64>("rx_unicast_total")[..],
            &[TimeMatrixCall::Fold(Timed::now(100u64))]
        );
        assert_eq!(
            &time_matrix_calls.drain::<u64>("rx_unicast_drop")[..],
            &[TimeMatrixCall::Fold(Timed::now(5u64))]
        );
        assert_eq!(
            &time_matrix_calls.drain::<u64>("tx_total")[..],
            &[TimeMatrixCall::Fold(Timed::now(50u64))]
        );
        assert_eq!(
            &time_matrix_calls.drain::<u64>("tx_drop")[..],
            &[TimeMatrixCall::Fold(Timed::now(2u64))]
        );
    }

    #[fuchsia::test]
    fn test_handle_periodic_telemetry_driver_specific_counters() {
        let mut test_helper = setup_test();
        let driver_mock_matrix_client = MockTimeMatrixClient::new();
        let logger = ClientIfaceCountersLogger::new(
            test_helper.monitor_svc_proxy.clone(),
            &test_helper.mock_time_matrix_client,
            driver_mock_matrix_client.clone(),
        );

        let mut handle_iface_created_fut = pin!(logger.handle_iface_created(IFACE_ID));
        assert_eq!(
            test_helper.run_and_handle_get_sme_telemetry(&mut handle_iface_created_fut),
            Poll::Pending
        );

        let mocked_inspect_configs = vec![
            fidl_stats::InspectCounterConfig {
                counter_id: Some(1),
                counter_name: Some("foo_counter".to_string()),
                ..Default::default()
            },
            fidl_stats::InspectCounterConfig {
                counter_id: Some(2),
                counter_name: Some("bar_counter".to_string()),
                ..Default::default()
            },
            fidl_stats::InspectCounterConfig {
                counter_id: Some(3),
                counter_name: Some("baz_counter".to_string()),
                ..Default::default()
            },
        ];
        let telemetry_support = fidl_stats::TelemetrySupport {
            inspect_counter_configs: Some(mocked_inspect_configs),
            ..Default::default()
        };
        assert_eq!(
            test_helper.run_and_respond_query_telemetry_support(
                &mut handle_iface_created_fut,
                Ok(&telemetry_support)
            ),
            Poll::Ready(())
        );

        let is_connected = true;
        let mut test_fut = pin!(logger.handle_periodic_telemetry(is_connected));
        let counter_stats = fidl_stats::IfaceCounterStats {
            driver_specific_counters: Some(vec![fidl_stats::UnnamedCounter { id: 1, count: 50 }]),
            connection_counters: Some(fidl_stats::ConnectionCounters {
                connection_id: Some(1),
                driver_specific_counters: Some(vec![
                    fidl_stats::UnnamedCounter { id: 2, count: 100 },
                    fidl_stats::UnnamedCounter { id: 3, count: 150 },
                    // This one is no-op because it's not registered in QueryTelemetrySupport
                    fidl_stats::UnnamedCounter { id: 4, count: 200 },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            test_helper.run_and_respond_iface_counter_stats_req(&mut test_fut, Ok(&counter_stats)),
            Poll::Ready(())
        );

        let time_matrix_calls = test_helper.mock_time_matrix_client.drain_calls();
        assert!(time_matrix_calls.is_empty());

        let mut driver_matrix_calls = driver_mock_matrix_client.drain_calls();
        assert_eq!(
            &driver_matrix_calls.drain::<u64>("foo_counter")[..],
            &[TimeMatrixCall::Fold(Timed::now(50))]
        );
        assert_eq!(
            &driver_matrix_calls.drain::<u64>("bar_counter")[..],
            &[TimeMatrixCall::Fold(Timed::now(100))]
        );
        assert_eq!(
            &driver_matrix_calls.drain::<u64>("baz_counter")[..],
            &[TimeMatrixCall::Fold(Timed::now(150))]
        );
    }

    #[fuchsia::test]
    fn test_handle_iface_destroyed() {
        let mut test_helper = setup_test();
        let driver_mock_matrix_client = MockTimeMatrixClient::new();
        let logger = ClientIfaceCountersLogger::new(
            test_helper.monitor_svc_proxy.clone(),
            &test_helper.mock_time_matrix_client,
            driver_mock_matrix_client.clone(),
        );

        // Transition to IfaceCreated state
        handle_iface_created(&mut test_helper, &logger);

        let mut handle_iface_destroyed_fut = pin!(logger.handle_iface_destroyed(IFACE_ID));
        assert_eq!(
            test_helper.exec.run_until_stalled(&mut handle_iface_destroyed_fut),
            Poll::Ready(())
        );

        let is_connected = true;
        let mut test_fut = pin!(logger.handle_periodic_telemetry(is_connected));
        assert_eq!(test_helper.exec.run_until_stalled(&mut test_fut), Poll::Ready(()));
        let telemetry_svc_stream = test_helper.telemetry_svc_stream.as_mut().unwrap();
        let mut telemetry_svc_req_fut = pin!(telemetry_svc_stream.try_next());
        // Verify that no telemetry request is made now that the iface is destroyed
        match test_helper.exec.run_until_stalled(&mut telemetry_svc_req_fut) {
            Poll::Ready(Ok(None)) => (),
            other => panic!("unexpected variant: {:?}", other),
        }
    }

    fn handle_iface_created<S: InspectSender>(
        test_helper: &mut TestHelper,
        logger: &ClientIfaceCountersLogger<S>,
    ) {
        let mut handle_iface_created_fut = pin!(logger.handle_iface_created(IFACE_ID));
        assert_eq!(
            test_helper.run_and_handle_get_sme_telemetry(&mut handle_iface_created_fut),
            Poll::Pending
        );
        let telemetry_support = fidl_stats::TelemetrySupport::default();
        assert_eq!(
            test_helper.run_and_respond_query_telemetry_support(
                &mut handle_iface_created_fut,
                Ok(&telemetry_support)
            ),
            Poll::Ready(())
        );
    }
}
