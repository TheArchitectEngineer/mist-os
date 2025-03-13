// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::diagnostics::TRACE_CATEGORY;
use crate::identity::ComponentIdentity;
use crate::logs::multiplex::PinStream;
use crate::logs::shared_buffer::{self, ContainerBuffer, LazyItem};
use crate::logs::socket::{Encoding, LogMessageSocket};
use crate::logs::stats::LogStreamStats;
use crate::logs::stored_message::StoredMessage;
use derivative::Derivative;
use diagnostics_data::{BuilderArgs, Data, LogError, Logs, LogsData, LogsDataBuilder};
use fidl_fuchsia_diagnostics::{
    Interest as FidlInterest, LogInterestSelector, Severity as FidlSeverity, StreamMode,
};
use fidl_fuchsia_logger::{LogSinkRequest, LogSinkRequestStream};
use fuchsia_async::condition::Condition;
use futures::future::{Fuse, FusedFuture};
use futures::prelude::*;
use futures::select;
use futures::stream::StreamExt;
use log::{debug, error, warn};
use selectors::SelectorExt;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use {fuchsia_async as fasync, fuchsia_trace as ftrace};

pub type OnInactive = Box<dyn Fn(&LogsArtifactsContainer) + Send + Sync>;

#[derive(Derivative)]
#[derivative(Debug)]
pub struct LogsArtifactsContainer {
    /// The source of logs in this container.
    pub identity: Arc<ComponentIdentity>,

    /// Inspect instrumentation.
    pub stats: Arc<LogStreamStats>,

    /// Buffer for all log messages.
    #[derivative(Debug = "ignore")]
    buffer: ContainerBuffer,

    /// Mutable state for the container.
    #[derivative(Debug = "ignore")]
    state: Arc<Condition<ContainerState>>,

    /// A callback which is called when the container is inactive i.e. has no channels, sockets or
    /// stored logs.
    #[derivative(Debug = "ignore")]
    on_inactive: Option<OnInactive>,
}

#[derive(Debug)]
struct ContainerState {
    /// Number of legacy sockets currently being drained for this component.  Sockets that use
    /// structured messages use the buffer's socket handling.
    num_active_legacy_sockets: u64,

    /// Number of LogSink channels currently being listened to for this component.
    num_active_channels: u64,

    /// Current interest for this component.
    interests: BTreeMap<Interest, usize>,

    is_initializing: bool,
}

#[derive(Debug, PartialEq)]
pub struct CursorItem {
    pub rolled_out: u64,
    pub message: Arc<StoredMessage>,
    pub identity: Arc<ComponentIdentity>,
}

impl Eq for CursorItem {}

impl Ord for CursorItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.message.timestamp().cmp(&other.message.timestamp())
    }
}

impl PartialOrd for CursorItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.message.timestamp().cmp(&other.message.timestamp()))
    }
}

impl LogsArtifactsContainer {
    pub fn new<'a>(
        identity: Arc<ComponentIdentity>,
        interest_selectors: impl Iterator<Item = &'a LogInterestSelector>,
        initial_interest: Option<FidlSeverity>,
        stats: Arc<LogStreamStats>,
        buffer: ContainerBuffer,
        on_inactive: Option<OnInactive>,
    ) -> Self {
        let mut interests = BTreeMap::new();
        if let Some(severity) = initial_interest {
            interests.insert(Interest::from(severity), 1);
        }
        let new = Self {
            identity,
            buffer,
            state: Arc::new(Condition::new(ContainerState {
                num_active_channels: 0,
                num_active_legacy_sockets: 0,
                interests,
                is_initializing: true,
            })),
            stats,
            on_inactive,
        };

        // there are no control handles so this won't notify anyone
        new.update_interest(interest_selectors, &[]);

        new
    }

    fn create_raw_cursor(
        &self,
        buffer_cursor: shared_buffer::Cursor,
    ) -> impl Stream<Item = CursorItem> {
        let identity = Arc::clone(&self.identity);
        buffer_cursor
            .enumerate()
            .scan((zx::BootInstant::ZERO, 0u64), move |(last_timestamp, rolled_out), (i, item)| {
                futures::future::ready(match item {
                    LazyItem::Next(message) => {
                        *last_timestamp = message.timestamp();
                        Some(Some(CursorItem {
                            message,
                            identity: Arc::clone(&identity),
                            rolled_out: *rolled_out,
                        }))
                    }
                    LazyItem::ItemsRolledOut(rolled_out_count, timestamp) => {
                        if i > 0 {
                            *rolled_out += rolled_out_count;
                        }
                        *last_timestamp = timestamp;
                        Some(None)
                    }
                })
            })
            .filter_map(future::ready)
    }

    /// Returns a stream of this component's log messages. These are the raw messages in FXT format.
    ///
    /// # Rolled out logs
    ///
    /// When messages are evicted from our internal buffers before a client can read them, they
    /// are counted as rolled out messages which gets appended to the metadata of the next message.
    /// If there is no next message, there is no way to know how many messages were rolled out.
    pub fn cursor_raw(&self, mode: StreamMode) -> PinStream<CursorItem> {
        let Some(buffer_cursor) = self.buffer.cursor(mode) else {
            return Box::pin(futures::stream::empty());
        };
        Box::pin(self.create_raw_cursor(buffer_cursor))
    }

    /// Returns a stream of this component's log messages.
    ///
    /// # Rolled out logs
    ///
    /// When messages are evicted from our internal buffers before a client can read them, they
    /// are counted as rolled out messages which gets appended to the metadata of the next message.
    /// If there is no next message, there is no way to know how many messages were rolled out.
    pub fn cursor(
        &self,
        mode: StreamMode,
        parent_trace_id: ftrace::Id,
    ) -> PinStream<Arc<LogsData>> {
        let Some(buffer_cursor) = self.buffer.cursor(mode) else {
            return Box::pin(futures::stream::empty());
        };
        let mut rolled_out_count = 0;
        Box::pin(self.create_raw_cursor(buffer_cursor).map(
            move |CursorItem { message, identity, rolled_out }| {
                rolled_out_count += rolled_out;
                let trace_id = ftrace::Id::random();
                let _trace_guard = ftrace::async_enter!(
                    trace_id,
                    TRACE_CATEGORY,
                    c"LogContainer::cursor.parse_message",
                    // An async duration cannot have multiple concurrent child async durations
                    // so we include the nonce as metadata to manually determine relationship.
                    "parent_trace_id" => u64::from(parent_trace_id),
                    "trace_id" => u64::from(trace_id)
                );
                match message.parse(&identity) {
                    Ok(m) => Arc::new(maybe_add_rolled_out_error(&mut rolled_out_count, m)),
                    Err(err) => {
                        let data = maybe_add_rolled_out_error(
                            &mut rolled_out_count,
                            LogsDataBuilder::new(BuilderArgs {
                                moniker: identity.moniker.clone(),
                                timestamp: message.timestamp(),
                                component_url: Some(identity.url.clone()),
                                severity: diagnostics_data::Severity::Warn,
                            })
                            .add_error(diagnostics_data::LogError::FailedToParseRecord(format!(
                                "{err:?}"
                            )))
                            .build(),
                        );
                        Arc::new(data)
                    }
                }
            },
        ))
    }

    /// Handle `LogSink` protocol on `stream`. Each socket received from the `LogSink` client is
    /// drained by a `Task` which is sent on `sender`. The `Task`s do not complete until their
    /// sockets have been closed.
    pub fn handle_log_sink(
        self: &Arc<Self>,
        stream: LogSinkRequestStream,
        scope: fasync::ScopeHandle,
    ) {
        {
            let mut guard = self.state.lock();
            guard.num_active_channels += 1;
            guard.is_initializing = false;
        }
        scope.spawn(Arc::clone(self).actually_handle_log_sink(stream, scope.clone()));
    }

    /// This function does not return until the channel is closed.
    async fn actually_handle_log_sink(
        self: Arc<Self>,
        mut stream: LogSinkRequestStream,
        scope: fasync::ScopeHandle,
    ) {
        let mut previous_interest_sent = None;
        debug!(identity:% = self.identity; "Draining LogSink channel.");

        let mut hanging_gets = Vec::new();
        let mut interest_changed = pin!(Fuse::terminated());

        loop {
            select! {
                next = stream.next() => {
                    let Some(next) = next else { break };
                    match next {
                        Ok(LogSinkRequest::Connect { socket, .. }) => {
                            // TODO(https://fxbug.dev/378977533): Add support for ingesting
                            // the legacy log format directly to the shared buffer.
                            let socket = fasync::Socket::from_socket(socket);
                            let log_stream = LogMessageSocket::new(socket, Arc::clone(&self.stats));
                            self.state.lock().num_active_legacy_sockets += 1;
                            scope.spawn(Arc::clone(&self).drain_messages(log_stream));
                        }
                        Ok(LogSinkRequest::ConnectStructured { socket, .. }) => {
                            self.buffer.add_socket(socket);
                        }
                        Ok(LogSinkRequest::WaitForInterestChange { responder }) => {
                            // If the interest has changed since we last reported it, we'll report
                            // it below.
                            hanging_gets.push(responder);
                        }
                        Err(e) => error!(identity:% = self.identity, e:%; "error handling log sink"),
                        Ok(LogSinkRequest::_UnknownMethod { .. }) => {}
                    }
                }
                _ = interest_changed => {}
            }

            if !hanging_gets.is_empty() {
                let min_interest = self.state.lock().min_interest();
                if Some(&min_interest) != previous_interest_sent.as_ref() {
                    // Send updates to all outstanding hanging gets.
                    for responder in hanging_gets.drain(..) {
                        let _ = responder.send(Ok(&min_interest));
                    }
                    interest_changed.set(Fuse::terminated());
                    previous_interest_sent = Some(min_interest);
                } else if interest_changed.is_terminated() {
                    // Set ourselves up to be woken when the interest changes.
                    let previous_interest_sent = previous_interest_sent.clone();
                    interest_changed.set(
                        self.state
                            .when(move |state| {
                                if previous_interest_sent != Some(state.min_interest()) {
                                    Poll::Ready(())
                                } else {
                                    Poll::Pending
                                }
                            })
                            .fuse(),
                    );
                }
            }
        }

        debug!(identity:% = self.identity; "LogSink channel closed.");
        self.state.lock().num_active_channels -= 1;
        self.check_inactive();
    }

    /// Drain a `LogMessageSocket` which wraps a socket from a component
    /// generating logs.
    pub async fn drain_messages<E>(self: Arc<Self>, mut log_stream: LogMessageSocket<E>)
    where
        E: Encoding + Unpin,
    {
        debug!(identity:% = self.identity; "Draining messages from a socket.");
        loop {
            match log_stream.next().await {
                Some(Ok(message)) => self.ingest_message(message),
                Some(Err(err)) => {
                    warn!(source:% = self.identity, err:%; "closing socket");
                    break;
                }
                None => break,
            }
        }
        debug!(identity:% = self.identity; "Socket closed.");
        self.state.lock().num_active_legacy_sockets -= 1;
        self.check_inactive();
    }

    /// Updates log stats in inspect and push the message onto the container's buffer.
    pub fn ingest_message(&self, message: StoredMessage) {
        self.stats.ingest_message(message.size(), message.severity());
        self.buffer.push_back(message.bytes());
    }

    /// Set the `Interest` for this component, notifying all active `LogSink/WaitForInterestChange`
    /// hanging gets with the new interset if it is a change from the previous interest.
    /// For any match that is also contained in `previous_selectors`, the previous values will be
    /// removed from the set of interests.
    pub fn update_interest<'a>(
        &self,
        interest_selectors: impl Iterator<Item = &'a LogInterestSelector>,
        previous_selectors: &[LogInterestSelector],
    ) {
        let mut new_interest = FidlInterest::default();
        let mut remove_interest = FidlInterest::default();
        for selector in interest_selectors {
            if self
                .identity
                .moniker
                .matches_component_selector(&selector.selector)
                .unwrap_or_default()
            {
                new_interest = selector.interest.clone();
                // If there are more matches, ignore them, we'll pick the first match.
                break;
            }
        }

        if let Some(previous_selector) = previous_selectors.iter().find(|s| {
            self.identity.moniker.matches_component_selector(&s.selector).unwrap_or_default()
        }) {
            remove_interest = previous_selector.interest.clone();
        }

        let mut state = self.state.lock();
        // Unfortunately we cannot use a match statement since `FidlInterest` doesn't derive Eq.
        // It does derive PartialEq though. All these branches will send an interest update if the
        // minimum interest changes after performing the required actions.
        if new_interest == FidlInterest::default() && remove_interest != FidlInterest::default() {
            state.erase(&remove_interest);
        } else if new_interest != FidlInterest::default()
            && remove_interest == FidlInterest::default()
        {
            state.push_interest(new_interest);
        } else if new_interest != FidlInterest::default()
            && remove_interest != FidlInterest::default()
        {
            state.erase(&remove_interest);
            state.push_interest(new_interest);
        } else {
            return;
        }

        for waker in state.drain_wakers() {
            waker.wake();
        }
    }

    /// Resets the `Interest` for this component, notifying all active
    /// `LogSink/WaitForInterestChange` hanging gets with the lowest interest found in the set of
    /// requested interests for all control handles.
    pub fn reset_interest(&self, interest_selectors: &[LogInterestSelector]) {
        for selector in interest_selectors {
            if self
                .identity
                .moniker
                .matches_component_selector(&selector.selector)
                .unwrap_or_default()
            {
                let mut state = self.state.lock();
                state.erase(&selector.interest);
                for waker in state.drain_wakers() {
                    waker.wake();
                }
                return;
            }
        }
    }

    /// Returns `true` if this container corresponds to a running component, or still has pending
    /// objects to drain.
    pub fn is_active(&self) -> bool {
        let state = self.state.lock();
        state.is_initializing
            || state.num_active_legacy_sockets > 0
            || state.num_active_channels > 0
            || self.buffer.is_active()
    }

    /// Called whenever there's a transition that means the component might no longer be active.
    fn check_inactive(&self) {
        if !self.is_active() {
            if let Some(on_inactive) = &self.on_inactive {
                on_inactive(self);
            }
        }
    }

    /// Stop accepting new messages, ensuring that pending Cursors return Poll::Ready(None) after
    /// consuming any messages received before this call.
    pub fn terminate(&self) {
        self.buffer.terminate();
    }

    #[cfg(test)]
    pub fn mark_stopped(&self) {
        self.state.lock().is_initializing = false;
        self.check_inactive();
    }
}

fn maybe_add_rolled_out_error(rolled_out_messages: &mut u64, mut msg: Data<Logs>) -> Data<Logs> {
    if *rolled_out_messages != 0 {
        // Add rolled out metadata
        msg.metadata
            .errors
            .get_or_insert(vec![])
            .push(LogError::RolledOutLogs { count: *rolled_out_messages });
    }
    *rolled_out_messages = 0;
    msg
}

impl ContainerState {
    /// Pushes the given `interest` to the set.
    fn push_interest(&mut self, interest: FidlInterest) {
        if interest != FidlInterest::default() {
            let count = self.interests.entry(interest.into()).or_insert(0);
            *count += 1;
        }
    }

    /// Removes the given `interest` from the set
    fn erase(&mut self, interest: &FidlInterest) {
        let interest = interest.clone().into();
        if let Some(count) = self.interests.get_mut(&interest) {
            if *count <= 1 {
                self.interests.remove(&interest);
            } else {
                *count -= 1;
            }
        }
    }

    /// Returns a copy of the lowest interest in the set. If the set is empty, an EMPTY interest is
    /// returned.
    fn min_interest(&self) -> FidlInterest {
        // btreemap: keys are sorted and ascending.
        self.interests.keys().next().map(|i| i.0.clone()).unwrap_or_default()
    }
}

#[derive(Debug, PartialEq)]
struct Interest(FidlInterest);

impl From<FidlInterest> for Interest {
    fn from(interest: FidlInterest) -> Interest {
        Interest(interest)
    }
}

impl From<FidlSeverity> for Interest {
    fn from(severity: FidlSeverity) -> Interest {
        Interest(FidlInterest { min_severity: Some(severity), ..Default::default() })
    }
}

impl std::ops::Deref for Interest {
    type Target = FidlInterest;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Eq for Interest {}

impl Ord for Interest {
    fn cmp(&self, other: &Self) -> Ordering {
        self.min_severity.cmp(&other.min_severity)
    }
}

impl PartialOrd for Interest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::shared_buffer::SharedBuffer;
    use fidl_fuchsia_diagnostics::{ComponentSelector, Severity, StringSelector};
    use fidl_fuchsia_logger::{LogSinkMarker, LogSinkProxy};
    use fuchsia_async::{Task, TestExecutor};
    use fuchsia_inspect as inspect;
    use fuchsia_inspect_derive::WithInspect;
    use moniker::ExtendedMoniker;

    fn initialize_container(
        severity: Option<Severity>,
        scope: fasync::ScopeHandle,
    ) -> (Arc<LogsArtifactsContainer>, LogSinkProxy) {
        let identity = Arc::new(ComponentIdentity::new(
            ExtendedMoniker::parse_str("/foo/bar").unwrap(),
            "fuchsia-pkg://test",
        ));
        let stats = Arc::new(
            LogStreamStats::default()
                .with_inspect(inspect::component::inspector().root(), identity.moniker.to_string())
                .expect("failed to attach component log stats"),
        );
        let buffer = SharedBuffer::new(1024 * 1024, Box::new(|_| {}));
        let container = Arc::new(LogsArtifactsContainer::new(
            identity,
            std::iter::empty(),
            severity,
            Arc::clone(&stats),
            buffer.new_container_buffer(Arc::new(vec!["a"].into()), stats),
            None,
        ));
        // Connect out LogSink under test and take its events channel.
        let (proxy, stream) = fidl::endpoints::create_proxy_and_stream::<LogSinkMarker>();
        container.handle_log_sink(stream, scope);
        (container, proxy)
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn update_interest() {
        // Sync path test (initial interest)
        let scope = fasync::Scope::new();
        let (container, log_sink) = initialize_container(None, scope.to_handle());

        // Get initial interest
        let initial_interest = log_sink.wait_for_interest_change().await.unwrap().unwrap();

        // Async (blocking) path test.
        assert_eq!(initial_interest.min_severity, None);
        let log_sink_clone = log_sink.clone();
        let mut interest_future =
            Task::spawn(async move { log_sink_clone.wait_for_interest_change().await });

        // The call should be blocked.
        assert!(TestExecutor::poll_until_stalled(&mut interest_future).await.is_pending());

        // We should see this interest update. This should unblock the hanging get.
        container.update_interest([interest(&["foo", "bar"], Some(Severity::Info))].iter(), &[]);

        // Verify we see the last interest we set.
        assert_eq!(interest_future.await.unwrap().unwrap().min_severity, Some(Severity::Info));
    }

    #[fuchsia::test]
    async fn initial_interest() {
        let scope = fasync::Scope::new();
        let (_container, log_sink) = initialize_container(Some(Severity::Info), scope.to_handle());
        let initial_interest = log_sink.wait_for_interest_change().await.unwrap().unwrap();
        assert_eq!(initial_interest.min_severity, Some(Severity::Info));
    }

    #[fuchsia::test]
    async fn interest_serverity_semantics() {
        let scope = fasync::Scope::new();
        let (container, log_sink) = initialize_container(None, scope.to_handle());
        let initial_interest = log_sink.wait_for_interest_change().await.unwrap().unwrap();
        assert_eq!(initial_interest.min_severity, None);
        // Set some interest.
        container.update_interest([interest(&["foo", "bar"], Some(Severity::Info))].iter(), &[]);
        assert_severity(&log_sink, Severity::Info).await;
        assert_interests(&container, [(Severity::Info, 1)]);

        // Sending a higher interest (WARN > INFO) has no visible effect, even if the new interest
        // (WARN) will be tracked internally until reset.
        container.update_interest([interest(&["foo", "bar"], Some(Severity::Warn))].iter(), &[]);
        assert_interests(&container, [(Severity::Info, 1), (Severity::Warn, 1)]);

        // Sending a lower interest (DEBUG < INFO) updates the previous one.
        container.update_interest([interest(&["foo", "bar"], Some(Severity::Debug))].iter(), &[]);
        assert_severity(&log_sink, Severity::Debug).await;
        assert_interests(
            &container,
            [(Severity::Debug, 1), (Severity::Info, 1), (Severity::Warn, 1)],
        );

        // Sending the same interest leads to tracking it twice, but no updates are sent since it's
        // the same minimum interest.
        container.update_interest([interest(&["foo", "bar"], Some(Severity::Debug))].iter(), &[]);
        assert_interests(
            &container,
            [(Severity::Debug, 2), (Severity::Info, 1), (Severity::Warn, 1)],
        );

        // The first reset does nothing, since the new minimum interest remains the same (we had
        // inserted twice, therefore we need to reset twice).
        container.reset_interest(&[interest(&["foo", "bar"], Some(Severity::Debug))]);
        assert_interests(
            &container,
            [(Severity::Debug, 1), (Severity::Info, 1), (Severity::Warn, 1)],
        );

        // The second reset causes a change in minimum interest -> now INFO.
        container.reset_interest(&[interest(&["foo", "bar"], Some(Severity::Debug))]);
        assert_severity(&log_sink, Severity::Info).await;
        assert_interests(&container, [(Severity::Info, 1), (Severity::Warn, 1)]);

        // If we pass a previous severity (INFO), then we undo it and set the new one (ERROR).
        // However, we get WARN since that's the minimum severity in the set.
        container.update_interest(
            [interest(&["foo", "bar"], Some(Severity::Error))].iter(),
            &[interest(&["foo", "bar"], Some(Severity::Info))],
        );
        assert_severity(&log_sink, Severity::Warn).await;
        assert_interests(&container, [(Severity::Error, 1), (Severity::Warn, 1)]);

        // When we reset warn, now we get ERROR since that's the minimum severity in the set.
        container.reset_interest(&[interest(&["foo", "bar"], Some(Severity::Warn))]);
        assert_severity(&log_sink, Severity::Error).await;
        assert_interests(&container, [(Severity::Error, 1)]);

        // When we reset ERROR , we get back to EMPTY since we have removed all interests from the
        // set.
        container.reset_interest(&[interest(&["foo", "bar"], Some(Severity::Error))]);
        assert_eq!(
            log_sink.wait_for_interest_change().await.unwrap().unwrap(),
            FidlInterest::default()
        );

        assert_interests(&container, []);
    }

    fn interest(moniker: &[&str], min_severity: Option<Severity>) -> LogInterestSelector {
        LogInterestSelector {
            selector: ComponentSelector {
                moniker_segments: Some(
                    moniker.iter().map(|s| StringSelector::ExactMatch(s.to_string())).collect(),
                ),
                ..Default::default()
            },
            interest: FidlInterest { min_severity, ..Default::default() },
        }
    }

    async fn assert_severity(proxy: &LogSinkProxy, severity: Severity) {
        assert_eq!(
            proxy.wait_for_interest_change().await.unwrap().unwrap().min_severity.unwrap(),
            severity
        );
    }

    fn assert_interests<const N: usize>(
        container: &LogsArtifactsContainer,
        severities: [(Severity, usize); N],
    ) {
        let mut expected_map = BTreeMap::new();
        expected_map.extend(IntoIterator::into_iter(severities).map(|(s, c)| {
            let interest = FidlInterest { min_severity: Some(s), ..Default::default() };
            (interest.into(), c)
        }));
        assert_eq!(expected_map, container.state.lock().interests);
    }
}
