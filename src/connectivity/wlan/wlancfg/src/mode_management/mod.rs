// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::client::connection_selection::ConnectionSelectionRequester;
use crate::client::roaming::local_roam_manager::RoamManager;
use crate::config_management::SavedNetworksManagerApi;
use crate::telemetry::{TelemetrySender, TimeoutSource};
use crate::util::listener;
use anyhow::Error;
use fuchsia_async as fasync;
use fuchsia_inspect::Node as InspectNode;
use fuchsia_inspect_contrib::inspect_insert;
use fuchsia_inspect_contrib::log::WriteInspect;
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::Future;
use std::borrow::Cow;
use std::convert::Infallible;
use std::sync::Arc;

pub mod device_monitor;
mod iface_manager;
pub mod iface_manager_api;
mod iface_manager_types;
pub mod phy_manager;
pub mod recovery;

pub const DEFECT_CHANNEL_SIZE: usize = 100;

pub fn create_iface_manager(
    phy_manager: Arc<Mutex<dyn phy_manager::PhyManagerApi>>,
    client_update_sender: listener::ClientListenerMessageSender,
    ap_update_sender: listener::ApListenerMessageSender,
    dev_monitor_proxy: fidl_fuchsia_wlan_device_service::DeviceMonitorProxy,
    saved_networks: Arc<dyn SavedNetworksManagerApi>,
    connection_selection_requester: ConnectionSelectionRequester,
    roam_manager: RoamManager,
    telemetry_sender: TelemetrySender,
    defect_sender: mpsc::Sender<Defect>,
    defect_receiver: mpsc::Receiver<Defect>,
    recovery_receiver: recovery::RecoveryActionReceiver,
    node: fuchsia_inspect::Node,
) -> (Arc<Mutex<iface_manager_api::IfaceManager>>, impl Future<Output = Result<Infallible, Error>>)
{
    let (sender, receiver) = mpsc::channel(0);
    let iface_manager_sender = Arc::new(Mutex::new(iface_manager_api::IfaceManager { sender }));
    let iface_manager = iface_manager::IfaceManagerService::new(
        phy_manager,
        client_update_sender,
        ap_update_sender,
        dev_monitor_proxy,
        saved_networks,
        connection_selection_requester,
        roam_manager,
        telemetry_sender,
        defect_sender,
        node,
    );
    let iface_manager_service = iface_manager::serve_iface_manager_requests(
        iface_manager,
        receiver,
        defect_receiver,
        recovery_receiver,
    );

    (iface_manager_sender, iface_manager_service)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PhyFailure {
    IfaceCreationFailure { phy_id: u16 },
    IfaceDestructionFailure { phy_id: u16 },
}

#[derive(Clone, Copy, Debug)]
pub enum IfaceFailure {
    CanceledScan { iface_id: u16 },
    FailedScan { iface_id: u16 },
    EmptyScanResults { iface_id: u16 },
    ApStartFailure { iface_id: u16 },
    ConnectionFailure { iface_id: u16 },
    Timeout { iface_id: u16, source: TimeoutSource },
}

// Interfaces will come and go and each one will receive a different ID.  The failures are
// ultimately all associated with a given PHY and we will be interested in tallying up how many
// of a given failure type a PHY has seen when making recovery decisions.  As such, only the
// IfaceFailure variant should be considered when determining equality.  The contained interface ID
// is useful only for associating a failure with a PHY.
impl PartialEq for IfaceFailure {
    fn eq(&self, other: &Self) -> bool {
        #[allow(
            clippy::match_like_matches_macro,
            reason = "mass allow for https://fxbug.dev/381896734"
        )]
        match (*self, *other) {
            (IfaceFailure::CanceledScan { .. }, IfaceFailure::CanceledScan { .. }) => true,
            (IfaceFailure::FailedScan { .. }, IfaceFailure::FailedScan { .. }) => true,
            (IfaceFailure::EmptyScanResults { .. }, IfaceFailure::EmptyScanResults { .. }) => true,
            (IfaceFailure::ApStartFailure { .. }, IfaceFailure::ApStartFailure { .. }) => true,
            (IfaceFailure::ConnectionFailure { .. }, IfaceFailure::ConnectionFailure { .. }) => {
                true
            }
            (IfaceFailure::Timeout { .. }, IfaceFailure::Timeout { .. }) => true,
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Defect {
    Phy(PhyFailure),
    Iface(IfaceFailure),
}

impl WriteInspect for Defect {
    fn write_inspect<'a>(&self, writer: &InspectNode, key: impl Into<Cow<'a, str>>) {
        match self {
            Defect::Phy(PhyFailure::IfaceCreationFailure { phy_id }) => {
                inspect_insert!(writer, var key: {IfaceCreationFailure: {phy_id: phy_id}})
            }
            Defect::Phy(PhyFailure::IfaceDestructionFailure { phy_id }) => {
                inspect_insert!(writer, var key: {IfaceDestructionFailure: {phy_id: phy_id}})
            }
            Defect::Iface(IfaceFailure::CanceledScan { iface_id }) => {
                inspect_insert!(writer, var key: {CanceledScan: {iface_id: iface_id}})
            }
            Defect::Iface(IfaceFailure::FailedScan { iface_id }) => {
                inspect_insert!(writer, var key: {FailedScan: {iface_id: iface_id}})
            }
            Defect::Iface(IfaceFailure::EmptyScanResults { iface_id }) => {
                inspect_insert!(writer, var key: {EmptyScanResults: {iface_id: iface_id}})
            }
            Defect::Iface(IfaceFailure::ApStartFailure { iface_id }) => {
                inspect_insert!(writer, var key: {ApStartFailure: {iface_id: iface_id}})
            }
            Defect::Iface(IfaceFailure::ConnectionFailure { iface_id }) => {
                inspect_insert!(writer, var key: {ConnectionFailure: {iface_id: iface_id}})
            }
            Defect::Iface(IfaceFailure::Timeout { iface_id, .. }) => {
                inspect_insert!(writer, var key: {Timeout: {iface_id: iface_id}})
            }
        }
    }
}

#[derive(Debug, PartialEq)]
struct Event<T: PartialEq> {
    value: T,
    time: fasync::MonotonicInstant,
}

impl<T: PartialEq> Event<T> {
    fn new(value: T, time: fasync::MonotonicInstant) -> Self {
        Event { value, time }
    }
}

pub struct EventHistory<T: PartialEq> {
    events: Vec<Event<T>>,
    retention_time: zx::MonotonicDuration,
}

impl<T: PartialEq> EventHistory<T> {
    fn new(retention_seconds: u32) -> Self {
        EventHistory {
            events: Vec::new(),
            retention_time: zx::MonotonicDuration::from_seconds(retention_seconds as i64),
        }
    }

    fn add_event(&mut self, value: T) {
        let curr_time = fasync::MonotonicInstant::now();
        self.events.push(Event::new(value, curr_time));
        self.retain_unexpired_events(curr_time);
    }

    fn event_count(&mut self, value: T) -> usize {
        let curr_time = fasync::MonotonicInstant::now();
        self.retain_unexpired_events(curr_time);
        self.events.iter().filter(|event| event.value == value).count()
    }

    fn time_since_last_event(&mut self, value: T) -> Option<zx::MonotonicDuration> {
        let curr_time = fasync::MonotonicInstant::now();
        self.retain_unexpired_events(curr_time);

        for event in self.events.iter().rev() {
            if event.value == value {
                return Some(curr_time - event.time);
            }
        }
        None
    }

    fn retain_unexpired_events(&mut self, curr_time: fasync::MonotonicInstant) {
        let oldest_allowed_time = curr_time - self.retention_time;
        self.events.retain(|event| event.time > oldest_allowed_time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async::TestExecutor;
    use rand::Rng;
    use test_util::{assert_gt, assert_lt};

    #[fuchsia::test]
    fn test_event_retention() {
        // Allow for events to be retained for at most 1s.
        let mut event_history = EventHistory::<()>::new(1);

        // Add events at 0, 1, 2, 2 and a little bit seconds, and 3s.
        event_history.events = vec![
            Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(0) },
            Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(1_000_000_000) },
            Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(2_000_000_000) },
            Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(2_000_000_001) },
            Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(3_000_000_000) },
        ];

        // Retain those events within the retention window based on a current time of 3s.
        event_history.retain_unexpired_events(fasync::MonotonicInstant::from_nanos(3_000_000_000));

        // It is expected that the events at 2 and a little bit seconds and 3s are retained while
        // the others are discarded.
        assert_eq!(
            event_history.events,
            vec![
                Event::<()> {
                    value: (),
                    time: fasync::MonotonicInstant::from_nanos(2_000_000_001)
                },
                Event::<()> {
                    value: (),
                    time: fasync::MonotonicInstant::from_nanos(3_000_000_000)
                },
            ]
        );
    }

    #[derive(Debug, PartialEq)]
    enum TestEnum {
        Foo,
        Bar,
    }

    #[fuchsia::test]
    fn test_time_since_last_event() {
        // An executor is required to enable querying time.
        let _exec = TestExecutor::new();

        // Allow events to be stored basically forever.  The goal here is to ensure that the
        // retention policy does not discard any of our events.
        let mut event_history = EventHistory::<TestEnum>::new(u32::MAX);

        // Add some events with known timestamps.
        let foo_time: i64 = 1_123_123_123;
        let bar_time: i64 = 2_222_222_222;
        event_history.events = vec![
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::from_nanos(foo_time) },
            Event { value: TestEnum::Bar, time: fasync::MonotonicInstant::from_nanos(bar_time) },
        ];

        // Get the time before and after the function calls were made.  This allows for some slack
        // in evaluating whether the time calculations are in the realm of accurate.
        let start_time = fasync::MonotonicInstant::now().into_nanos();
        let time_since_foo =
            event_history.time_since_last_event(TestEnum::Foo).expect("Foo was not retained");
        let time_since_bar =
            event_history.time_since_last_event(TestEnum::Bar).expect("Bar was not retained");
        let end_time = fasync::MonotonicInstant::now().into_nanos();

        // Make sure the returned durations are within bounds.
        assert_lt!(time_since_foo.into_nanos(), end_time - foo_time);
        assert_gt!(time_since_foo.into_nanos(), start_time - foo_time);

        assert_lt!(time_since_bar.into_nanos(), end_time - bar_time);
        assert_gt!(time_since_bar.into_nanos(), start_time - bar_time);
    }

    #[fuchsia::test]
    fn test_time_since_last_event_retention() {
        // An executor is required to enable querying time.
        let _exec = TestExecutor::new();

        // Set the retention time to slightly less than the current time.  This number will be
        // positive.  Since it will occupy the positive range of i64, it is safe to cast it as u32.
        let curr_time_seconds = fasync::MonotonicInstant::now().into_nanos() / 1_000_000_000;
        let mut event_history = EventHistory::<()>::new((curr_time_seconds - 1) as u32);

        // Put in an event at time zero so that it will not be retained when querying recent
        // events.
        event_history
            .events
            .push(Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(0) });

        assert_eq!(event_history.time_since_last_event(()), None);
    }
    #[fuchsia::test]
    fn test_add_event() {
        // An executor is required to enable querying time.
        let _exec = TestExecutor::new();
        let mut event_history = EventHistory::<()>::new(u32::MAX);

        // Add a few events
        let num_events = 3;
        let start_time = fasync::MonotonicInstant::now().into_nanos();
        for _ in 0..num_events {
            event_history.add_event(());
        }
        let end_time = fasync::MonotonicInstant::now().into_nanos();

        // All three of the recent events should have been retained.
        assert_eq!(event_history.events.len(), num_events);

        // Verify that all of the even timestamps are within range.
        for event in event_history.events {
            let event_time = event.time.into_nanos();
            assert_lt!(event_time, end_time);
            assert_gt!(event_time, start_time);
        }
    }

    #[fuchsia::test]
    fn test_add_event_retention() {
        // An executor is required to enable querying time.
        let _exec = TestExecutor::new();

        // Set the retention time to slightly less than the current time.  This number will be
        // positive.  Since it will occupy the positive range of i64, it is safe to cast it as u32.
        let curr_time_seconds = fasync::MonotonicInstant::now().into_nanos() / 1_000_000_000;
        let mut event_history = EventHistory::<()>::new((curr_time_seconds - 1) as u32);

        // Put in an event at time zero so that it will not be retained when querying recent
        // events.
        event_history
            .events
            .push(Event::<()> { value: (), time: fasync::MonotonicInstant::from_nanos(0) });

        // Add an event and observe that the event from time 0 has been removed.
        let start_time = fasync::MonotonicInstant::now().into_nanos();
        event_history.add_event(());
        assert_eq!(event_history.events.len(), 1);

        // Add a couple more events.
        event_history.add_event(());
        event_history.add_event(());
        let end_time = fasync::MonotonicInstant::now().into_nanos();

        // All three of the recent events should have been retained.
        assert_eq!(event_history.events.len(), 3);

        // Verify that all of the even timestamps are within range.
        for event in event_history.events {
            let event_time = event.time.into_nanos();
            assert_lt!(event_time, end_time);
            assert_gt!(event_time, start_time);
        }
    }

    #[fuchsia::test]
    fn test_event_count() {
        // An executor is required to enable querying time.
        let _exec = TestExecutor::new();
        let mut event_history = EventHistory::<TestEnum>::new(u32::MAX);

        event_history.events = vec![
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::from_nanos(0) },
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::from_nanos(1) },
            Event { value: TestEnum::Bar, time: fasync::MonotonicInstant::from_nanos(2) },
            Event { value: TestEnum::Bar, time: fasync::MonotonicInstant::from_nanos(3) },
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::from_nanos(4) },
        ];

        assert_eq!(event_history.event_count(TestEnum::Foo), 3);
        assert_eq!(event_history.event_count(TestEnum::Bar), 2);
    }

    #[fuchsia::test]
    fn test_event_count_retention() {
        // An executor is required to enable querying time.
        let _exec = TestExecutor::new();

        // Set the retention time to slightly less than the current time.  This number will be
        // positive.  Since it will occupy the positive range of i64, it is safe to cast it as u32.
        let curr_time_seconds = fasync::MonotonicInstant::now().into_nanos() / 1_000_000_000;
        let mut event_history = EventHistory::<TestEnum>::new((curr_time_seconds - 1) as u32);

        event_history.events = vec![
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::from_nanos(0) },
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::from_nanos(0) },
            Event { value: TestEnum::Bar, time: fasync::MonotonicInstant::now() },
            Event { value: TestEnum::Bar, time: fasync::MonotonicInstant::now() },
            Event { value: TestEnum::Foo, time: fasync::MonotonicInstant::now() },
        ];

        assert_eq!(event_history.event_count(TestEnum::Foo), 1);
        assert_eq!(event_history.event_count(TestEnum::Bar), 2);
    }

    #[fuchsia::test]
    fn test_failure_equality() {
        let mut rng = rand::thread_rng();
        assert_eq!(
            IfaceFailure::CanceledScan { iface_id: rng.gen::<u16>() },
            IfaceFailure::CanceledScan { iface_id: rng.gen::<u16>() }
        );
        assert_eq!(
            IfaceFailure::FailedScan { iface_id: rng.gen::<u16>() },
            IfaceFailure::FailedScan { iface_id: rng.gen::<u16>() }
        );
        assert_eq!(
            IfaceFailure::EmptyScanResults { iface_id: rng.gen::<u16>() },
            IfaceFailure::EmptyScanResults { iface_id: rng.gen::<u16>() }
        );
        assert_eq!(
            IfaceFailure::ApStartFailure { iface_id: rng.gen::<u16>() },
            IfaceFailure::ApStartFailure { iface_id: rng.gen::<u16>() }
        );
        assert_eq!(
            IfaceFailure::ConnectionFailure { iface_id: rng.gen::<u16>() },
            IfaceFailure::ConnectionFailure { iface_id: rng.gen::<u16>() }
        );
        assert_eq!(
            IfaceFailure::Timeout { iface_id: rng.gen::<u16>(), source: TimeoutSource::Scan },
            IfaceFailure::Timeout { iface_id: rng.gen::<u16>(), source: TimeoutSource::ApStart }
        );
    }
}
