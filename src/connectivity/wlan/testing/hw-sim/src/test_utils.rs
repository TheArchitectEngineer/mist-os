// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::event::{self, Handler};
use crate::netdevice_helper;
use crate::wlancfg_helper::{start_ap_and_wait_for_confirmation, NetworkConfigBuilder};
use fidl::endpoints::{create_endpoints, create_proxy, Proxy};
use fuchsia_async::{DurationExt, MonotonicInstant, TimeoutExt, Timer};
use fuchsia_component::client::{connect_to_protocol, connect_to_protocol_at};
use zx::prelude::*;

use futures::channel::oneshot;
use futures::{FutureExt, StreamExt};
use ieee80211::{MacAddr, MacAddrBytes};
use log::{debug, info, warn};
use realm_client::{extend_namespace, InstalledNamespace};
use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use test_realm_helpers::tracing::Tracing;
use wlan_common::test_utils::ExpectWithin;
use wlantap_client::Wlantap;
use {
    fidl_fuchsia_driver_test as fidl_driver_test, fidl_fuchsia_wlan_policy as fidl_policy,
    fidl_fuchsia_wlan_tap as wlantap, fidl_test_wlan_realm as fidl_realm,
};

/// Percent of a timeout duration past which we log a warning.
const TIMEOUT_WARN_THRESHOLD: f64 = 0.8;

// Struct that allows a test suite to interact with the test realm.
//
// If the test suite needs to connect to a protocol exposed by the test realm, it MUST use the
// context's realm_proxy and cannot use fuchsia_component::client::connect_to_protocol.
//
// Similarly, if the test suite needs to connect to /dev hosted by the test realm, it must use the
// context's devfs. There is currently no way to access any other directories in the test realm. If
// the test suite needs to access any other directories, the test realm factory implementation and
// FIDL API will need to be changed.
//
// Example:
//
// // Create a new test realm context
// let ctx = ctx::new(fidl_realm::WlanConfig{ ..Default::default() };
//
// // Connect to a protocol
// let protocol_proxy = ctx.test_realm_proxy()
//   .connect_to_protocol::<fidl_fuchsia_protocol::Protocol>()
//   .await?;
//
// // Connect to dev/class/network in the test realm
// let (directory, directory_server) =
//      create_proxy::<fidl_fuchsia_io::DirectoryMarker>();
//  fdio::service_connect_at(
//     ctx.devfs().as_channel().as_ref(),
//     "class/network",
//     directory_server.into_channel(),
//  )?;
pub struct TestRealmContext {
    // The test namespace, which allows the test suite to connect to protocols exposed by
    // the test realm.
    test_ns: InstalledNamespace,

    // A directory proxy connected to "/dev" in the test realm.
    devfs: fidl_fuchsia_io::DirectoryProxy,
}

impl TestRealmContext {
    // Connect to the test realm factory to create and start a new test realm and return the test
    // realm context. This will also start the driver test realm.
    //
    // Panics if any errors occur when the realm factory is being created.
    pub async fn new(config: fidl_realm::WlanConfig) -> Arc<Self> {
        let realm_factory = connect_to_protocol::<fidl_realm::RealmFactoryMarker>()
            .expect("Could not connect to realm factory protocol");

        let (dict_client, dict_server) = create_endpoints();
        let (devfs_proxy, devfs_server) = create_proxy();

        // Create the test realm for this test. This returns a
        // `fuchsia.component.sandbox/Dictionary`, which is then consumed by `extend_namespace`
        // to turn it into a directory installed in this component's namespace at
        // `test_ns.prefix()`.
        let options = fidl_realm::RealmOptions {
            devfs_server_end: Some(devfs_server),
            wlan_config: Some(config),
            ..Default::default()
        };
        let _ = realm_factory
            .create_realm2(options, dict_server)
            .await
            .expect("Could not create realm");
        let test_ns =
            extend_namespace(realm_factory, dict_client).await.expect("failed to extend ns");

        // Start the driver test realm
        let driver_test_realm_proxy =
            connect_to_protocol_at::<fidl_driver_test::RealmMarker>(&test_ns)
                .expect("Failed to connect to driver test realm");

        let (pkg_client, pkg_server) = create_endpoints();
        fuchsia_fs::directory::open_channel_in_namespace(
            "/pkg",
            fidl_fuchsia_io::PERM_READABLE | fidl_fuchsia_io::PERM_EXECUTABLE,
            pkg_server,
        )
        .expect("Could not open /pkg");

        let test_component = fidl_fuchsia_component_resolution::Component {
            package: Some(fidl_fuchsia_component_resolution::Package {
                directory: Some(pkg_client),
                ..Default::default()
            }),
            ..Default::default()
        };

        driver_test_realm_proxy
            .start(fidl_driver_test::RealmArgs {
                test_component: Some(test_component),
                ..Default::default()
            })
            .await
            .expect("FIDL error when starting driver test realm")
            .expect("Driver test realm server returned an error");

        Arc::new(Self { test_ns, devfs: devfs_proxy })
    }

    pub fn test_ns_prefix(&self) -> &str {
        self.test_ns.prefix()
    }

    pub fn devfs(&self) -> &fidl_fuchsia_io::DirectoryProxy {
        &self.devfs
    }
}

type EventStream = wlantap::WlantapPhyEventStream;
pub struct TestHelper {
    ctx: Arc<TestRealmContext>,
    _tracing: Option<Tracing>,
    netdevice_task_handles: Vec<fuchsia_async::Task<()>>,
    _wlantap: Wlantap,
    proxy: Arc<wlantap::WlantapPhyProxy>,
    event_stream: Option<EventStream>,
}
struct TestHelperFuture<H, F>
where
    H: Handler<(), wlantap::WlantapPhyEvent>,
    F: Future + Unpin,
{
    event_stream: Option<EventStream>,
    handler: H,
    future: F,
}
impl<H, F> Unpin for TestHelperFuture<H, F>
where
    H: Handler<(), wlantap::WlantapPhyEvent>,
    F: Future + Unpin,
{
}
impl<H, F> Future for TestHelperFuture<H, F>
where
    H: Handler<(), wlantap::WlantapPhyEvent>,
    F: Future + Unpin,
{
    type Output = (F::Output, EventStream);
    /// Any events that accumulated in the |event_stream| since last poll will be passed to
    /// |event_handler| before the |main_future| is polled
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let helper = &mut *self;
        let stream = helper.event_stream.as_mut().unwrap();
        while let Poll::Ready(optional_result) = stream.poll_next_unpin(cx) {
            let event = optional_result
                .expect("Unexpected end of the WlantapPhy event stream")
                .expect("WlantapPhy event stream returned an error");
            helper.handler.call(&mut (), &event);
        }
        match helper.future.poll_unpin(cx) {
            Poll::Pending => {
                debug!("Main future poll response is pending. Waiting for completion.");
                Poll::Pending
            }
            Poll::Ready(x) => Poll::Ready((x, helper.event_stream.take().unwrap())),
        }
    }
}
impl TestHelper {
    // Create a client TestHelper with a new TestRealmContext.
    // NOTE: if a test case creates multiple TestHelpers that should all share the same test realm,
    // it should use TestHelper::begin_test_with_context.
    pub async fn begin_test(
        phy_config: wlantap::WlantapPhyConfig,
        realm_config: fidl_realm::WlanConfig,
    ) -> Self {
        let ctx = TestRealmContext::new(realm_config).await;
        Self::begin_test_with_context(ctx, phy_config).await
    }

    // Create client TestHelper with a given TestRealmContext.
    // If a test case creates multiple TestHelpers that must refer to the same instance of WLAN
    // components, then all TestHelpers must use a copy of the same TestRealmContext.
    //
    // Example:
    //
    // // Create a new test realm context
    // let ctx = TestRealmContext::new(fidl_realm::WlanConfig{ ..Default::default() };
    //
    // // Create both helpers with copies of the same context
    // let helper1 = TestHelper::begin_test_with_context(
    //    ctx.clone(),
    //    default_wlantap_client_config(),
    // ).await;
    //
    // let helper2 = TestHelper::begin_test_with_context(
    //    ctx.clone(),
    //    default_wlantap_client_config()).await;
    pub async fn begin_test_with_context(
        ctx: Arc<TestRealmContext>,
        config: wlantap::WlantapPhyConfig,
    ) -> Self {
        let mut helper = TestHelper::create_phy_and_helper(config, ctx).await;
        helper.wait_for_wlan_softmac_start().await;
        helper
    }

    // Create an AP TestHelper with a new TestRealmContext.
    // NOTE: if a test case creates multiple TestHelpers that should all share the same test realm,
    // it should use TestHelper::begin_ap_test_with_context.
    pub async fn begin_ap_test(
        phy_config: wlantap::WlantapPhyConfig,
        network_config: NetworkConfigBuilder,
        realm_config: fidl_realm::WlanConfig,
    ) -> Self {
        let ctx = TestRealmContext::new(realm_config).await;
        Self::begin_ap_test_with_context(ctx, phy_config, network_config).await
    }

    // Create AP TestHelper with a given TestRealmContext.
    // If a test case creates multiple TestHelpers that must refer to the same instance of WLAN
    // components, then all TestHelpers must use a copy of the same TestRealmContext.
    //
    // Example:
    //
    // // Create a new test realm context
    // let ctx = TestRealmContext::new(fidl_realm::WlanConfig{ ..Default::default() };
    //
    // // Create both helpers with copies of the same context
    // let helper1 = TestHelper::begin_ap_test_with_context(
    //    ctx.clone(),
    //    default_wlantap_client_config(),
    //    network_config1,
    // ).await;
    //
    // let helper2 = TestHelper::begin_ap_test_with_context(
    //    ctx.clone(),
    //    default_wlantap_client_config(),
    //    network_config2
    // ).await;
    pub async fn begin_ap_test_with_context(
        ctx: Arc<TestRealmContext>,
        config: wlantap::WlantapPhyConfig,
        network_config: NetworkConfigBuilder,
    ) -> Self {
        let mut helper = TestHelper::create_phy_and_helper(config, ctx).await;
        start_ap_and_wait_for_confirmation(helper.ctx.test_ns_prefix(), network_config).await;
        helper.wait_for_wlan_softmac_start().await;
        helper
    }

    async fn create_phy_and_helper(
        config: wlantap::WlantapPhyConfig,
        ctx: Arc<TestRealmContext>,
    ) -> Self {
        let tracing = Tracing::create_and_initialize_tracing(ctx.test_ns_prefix())
            .await
            .map_err(|e| warn!("{e:?}"))
            .ok();

        // Trigger creation of wlantap serviced phy and iface for testing.
        let wlantap =
            Wlantap::open_from_devfs(&ctx.devfs).await.expect("Failed to open wlantapctl");
        let proxy = wlantap.create_phy(config).await.expect("Failed to create wlantap PHY");
        let event_stream = Some(proxy.take_event_stream());
        TestHelper {
            ctx,
            _tracing: tracing,
            netdevice_task_handles: vec![],
            _wlantap: wlantap,
            proxy: Arc::new(proxy),
            event_stream,
        }
    }

    async fn wait_for_wlan_softmac_start(&mut self) {
        let (sender, receiver) = oneshot::channel::<()>();
        self.run_until_complete_or_timeout(
            zx::MonotonicDuration::from_seconds(120),
            "receive a WlanSoftmacStart event",
            event::on_start_mac(event::once(|_, _| sender.send(()))),
            receiver,
        )
        .await
        .unwrap_or_else(|oneshot::Canceled| panic!());
    }

    /// Returns a clone of the `Arc<wlantap::WlantapPhyProxy>` as a convenience for passing
    /// the proxy to futures. Tests must drop every `Arc<wlantap::WlantapPhyProxy>` returned from this
    /// method before dropping the TestHelper. Otherwise, TestHelper::drop() cannot synchronously
    /// block on WlantapPhy.Shutdown().
    pub fn proxy(&self) -> Arc<wlantap::WlantapPhyProxy> {
        Arc::clone(&self.proxy)
    }

    pub fn test_ns_prefix(&self) -> &str {
        self.ctx.test_ns_prefix()
    }

    pub fn devfs(&self) -> &fidl_fuchsia_io::DirectoryProxy {
        self.ctx.devfs()
    }

    pub async fn start_netdevice_session(
        &mut self,
        mac: MacAddr,
    ) -> (netdevice_client::Session, netdevice_client::Port) {
        let mac = fidl_fuchsia_net::MacAddress { octets: mac.to_array() };
        let (client, port) = netdevice_helper::create_client(self.devfs(), mac)
            .await
            .expect("failed to create netdevice client");
        let (session, task_handle) = netdevice_helper::start_session(client, port).await;
        self.netdevice_task_handles.push(task_handle);
        (session, port)
    }

    /// Will run the main future until it completes or when it has run past the specified duration.
    /// Note that any events that are observed on the event stream will be passed to the
    /// |event_handler| closure first before making progress on the main future.
    /// So if a test generates many events each of which requires significant computational time in
    /// the event handler, the main future may not be able to complete in time.
    pub async fn run_until_complete_or_timeout<H, F>(
        &mut self,
        timeout: zx::MonotonicDuration,
        context: impl Display,
        handler: H,
        future: F,
    ) -> F::Output
    where
        H: Handler<(), wlantap::WlantapPhyEvent>,
        F: Future + Unpin,
    {
        info!("Running main future until completion or timeout with event handler: {}", context);
        let start_time = zx::MonotonicInstant::get();
        let (item, stream) = TestHelperFuture {
            event_stream: Some(self.event_stream.take().unwrap()),
            handler,
            future,
        }
        .expect_within(timeout, format!("Main future timed out: {}", context))
        .await;
        let end_time = zx::MonotonicInstant::get();
        let elapsed = end_time - start_time;
        let elapsed_seconds = elapsed.into_seconds_f64();
        let elapsed_ratio = elapsed_seconds / timeout.into_seconds_f64();
        if elapsed_ratio < TIMEOUT_WARN_THRESHOLD {
            info!("Main future completed in {:.2} seconds: {}", elapsed_seconds, context);
        } else {
            warn!(
                "Main future completed in {:.2} seconds ({:.1}% of timeout): {}",
                elapsed_seconds,
                elapsed_ratio * 100.,
                context,
            );
        }
        self.event_stream = Some(stream);
        item
    }
}
impl Drop for TestHelper {
    fn drop(&mut self) {
        // Drop each fuchsia_async::Task driving each
        // netdevice_client::Session in the reverse order the test
        // created them.
        while let Some(task_handle) = self.netdevice_task_handles.pop() {
            drop(task_handle);
        }

        // Create a placeholder proxy to swap into place of self.proxy. This allows this
        // function to create a synchronous proxy from the real proxy.
        let (placeholder_proxy, _server_end) =
            fidl::endpoints::create_proxy::<wlantap::WlantapPhyMarker>();
        let mut proxy = Arc::new(placeholder_proxy);
        std::mem::swap(&mut self.proxy, &mut proxy);

        // Drop the event stream so the WlantapPhyProxy can be converted
        // back into a channel. Conversion from a proxy into a channel fails
        // otherwise.
        let event_stream = self.event_stream.take();
        drop(event_stream);

        let sync_proxy = wlantap::WlantapPhySynchronousProxy::new(fidl::Channel::from_handle(
            // Arc::into_inner() should succeed in a properly constructed test. Using a WlantapPhyProxy
            // returned from TestHelper beyond the lifetime of TestHelper is not supported.
            Arc::<wlantap::WlantapPhyProxy>::into_inner(proxy)
                .expect("Outstanding references to WlantapPhyProxy! Failed to drop TestHelper.")
                .into_channel()
                .expect("failed to get fidl::AsyncChannel from proxy")
                .into_zx_channel()
                .into_handle(),
        ));

        // TODO(b/307808624): At this point in the shutdown, we should
        // stop wlancfg first and destroy all ifaces through
        // fuchsia.wlan.device.service/DeviceMonitor.DestroyIface().
        // This test framework does not currently support stopping
        // individual components. If instead we drop the
        // TestRealmProxy, and thus stop both wlancfg and
        // wlandevicemonitor, wlandevicemonitor which will drop the
        // GenericSme channel before graceful destruction of the
        // iface. Dropping the GenericSme channel for an existing
        // iface is considered an error because doing so prevents
        // future communication with the iface.
        //
        // In lieu of stopping wlancfg first, we instead shutdown the
        // phy device via WlantapPhy.Shutdown() which will block until
        // both the phy and any remaining ifaces are shutdown. We
        // first shutdown the phy to prevent any automated CreateIface
        // calls from wlancfg after removing the iface.
        sync_proxy
            .shutdown(zx::MonotonicInstant::INFINITE)
            .expect("Failed to shutdown WlantapPhy gracefully.");
    }
}

pub struct RetryWithBackoff {
    deadline: MonotonicInstant,
    prev_delay: zx::MonotonicDuration,
    next_delay: zx::MonotonicDuration,
    max_delay: zx::MonotonicDuration,
}
impl RetryWithBackoff {
    pub fn new(timeout: zx::MonotonicDuration) -> Self {
        RetryWithBackoff {
            deadline: MonotonicInstant::after(timeout),
            prev_delay: zx::MonotonicDuration::from_millis(0),
            next_delay: zx::MonotonicDuration::from_millis(1),
            max_delay: zx::MonotonicDuration::INFINITE,
        }
    }
    pub fn infinite_with_max_interval(max_delay: zx::MonotonicDuration) -> Self {
        Self {
            deadline: MonotonicInstant::INFINITE,
            max_delay,
            ..Self::new(zx::MonotonicDuration::from_nanos(0))
        }
    }

    /// Return Err if the deadline was exceeded when this function was called.
    /// Otherwise, sleep for a little longer (following Fibonacci series) or up
    /// to the deadline, whichever is soonest. If a sleep occurred, this function
    /// returns Ok. The value contained in both Ok and Err is the zx::MonotonicDuration
    /// until or after the deadline when the function returns.
    async fn sleep_unless_after_deadline_(
        &mut self,
        verbose: bool,
    ) -> Result<zx::MonotonicDuration, zx::MonotonicDuration> {
        // Add an inner scope up to just after Timer::new to ensure all
        // time assignments are dropped after the sleep occurs. This
        // prevents misusing them after the sleep since they are all
        // no longer correct after the clock moves.
        {
            if MonotonicInstant::after(zx::MonotonicDuration::from_millis(0)) > self.deadline {
                if verbose {
                    info!("Skipping sleep. Deadline exceeded.");
                }
                return Err(self.deadline - MonotonicInstant::now());
            }

            let sleep_deadline =
                std::cmp::min(MonotonicInstant::after(self.next_delay), self.deadline);
            if verbose {
                let micros = sleep_deadline.into_nanos() / 1_000;
                info!("Sleeping until {}.{} 😴", micros / 1_000_000, micros % 1_000_000);
            }

            Timer::new(sleep_deadline).await;
        }

        // If the next delay interval exceeds max_delay (even if by overflow),
        // then saturate at max_delay.
        if self.next_delay < self.max_delay {
            let next_delay = std::cmp::min(
                self.max_delay,
                zx::MonotonicDuration::from_nanos(
                    self.prev_delay.into_nanos().saturating_add(self.next_delay.into_nanos()),
                ),
            );
            self.prev_delay = self.next_delay;
            self.next_delay = next_delay;
        }

        Ok(self.deadline - MonotonicInstant::now())
    }

    pub async fn sleep_unless_after_deadline(
        &mut self,
    ) -> Result<zx::MonotonicDuration, zx::MonotonicDuration> {
        self.sleep_unless_after_deadline_(false).await
    }

    pub async fn sleep_unless_after_deadline_verbose(
        &mut self,
    ) -> Result<zx::MonotonicDuration, zx::MonotonicDuration> {
        self.sleep_unless_after_deadline_(true).await
    }
}

/// TODO(https://fxbug.dev/42164608): This function strips the `timestamp_nanos` field
/// from each `fidl_fuchsia_wlan_policy::ScanResult` entry since the `timestamp_nanos`
/// field is undefined.
pub fn strip_timestamp_nanos_from_scan_results(
    mut scan_result_list: Vec<fidl_fuchsia_wlan_policy::ScanResult>,
) -> Vec<fidl_fuchsia_wlan_policy::ScanResult> {
    for scan_result in &mut scan_result_list {
        scan_result
            .entries
            .as_mut()
            .unwrap()
            .sort_by(|a, b| a.bssid.as_ref().unwrap().cmp(&b.bssid.as_ref().unwrap()));
        for entry in scan_result.entries.as_mut().unwrap() {
            // TODO(https://fxbug.dev/42164608): Strip timestamp_nanos since it's not implemented.
            entry.timestamp_nanos.take();
        }
    }
    scan_result_list
}

/// Sort a list of scan results by the `id` and `bssid` fields.
///
/// This function will panic if either of the `id` or `entries` fields
/// are `None`.
pub fn sort_policy_scan_result_list(
    mut scan_result_list: Vec<fidl_fuchsia_wlan_policy::ScanResult>,
) -> Vec<fidl_fuchsia_wlan_policy::ScanResult> {
    scan_result_list
        .sort_by(|a, b| a.id.as_ref().expect("empty id").cmp(&b.id.as_ref().expect("empty id")));
    scan_result_list
}

/// Returns a map with the scan results returned by the policy layer. The map is
/// keyed by the `id` field of each `fidl_fuchsia_policy::ScanResult`.
///
/// This function will panic if the `id` field is ever `None` or if policy returns
/// the same `id` twice. Both of these are invariants we expect the policy layer
/// to uphold.
pub async fn policy_scan_for_networks<'a>(
    client_controller: fidl_policy::ClientControllerProxy,
) -> Vec<fidl_policy::ScanResult> {
    // Request a scan from the policy layer.
    let (scan_proxy, server_end) = create_proxy();
    client_controller.scan_for_networks(server_end).expect("requesting scan");
    let mut scan_result_list = Vec::new();
    loop {
        let proxy_result = scan_proxy.get_next().await.expect("getting scan results");
        let next_scan_result_list = proxy_result.expect("scanning failed");
        if next_scan_result_list.is_empty() {
            break;
        }
        scan_result_list.extend(next_scan_result_list);
    }
    sort_policy_scan_result_list(strip_timestamp_nanos_from_scan_results(scan_result_list))
}

/// This function returns `Ok(r)`, where `r` is the return value from `main_future`,
/// if `main_future` completes before the `timeout` duration. Otherwise, `Err(())` is returned.
pub async fn timeout_after<R, F: Future<Output = R> + Unpin>(
    timeout: zx::MonotonicDuration,
    main_future: &mut F,
) -> Result<R, ()> {
    async { Ok(main_future.await) }.on_timeout(timeout.after_now(), || Err(())).await
}
