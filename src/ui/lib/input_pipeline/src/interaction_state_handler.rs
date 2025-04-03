// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#![cfg(fuchsia_api_level_at_least = "HEAD")]
use crate::input_device;
use crate::input_handler::{InputHandlerStatus, UnhandledInputHandler};
use anyhow::{Context, Error};
use async_trait::async_trait;
use async_utils::hanging_get::server::{HangingGet, Publisher, Subscriber};
use fidl_fuchsia_input_interaction::{
    NotifierRequest, NotifierRequestStream, NotifierWatchStateResponder, State,
};
use fidl_fuchsia_power_system::{ActivityGovernorMarker, ActivityGovernorProxy};
use fuchsia_async::{Task, Timer};
use fuchsia_component::client::connect_to_protocol;

use fuchsia_inspect::health::Reporter;
use futures::StreamExt;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct LeaseHolder {
    activity_governor: ActivityGovernorProxy,
    wake_lease: Option<zx::EventPair>,
}

impl LeaseHolder {
    async fn new(activity_governor: ActivityGovernorProxy) -> Result<Self, Error> {
        let wake_lease = activity_governor
            .take_wake_lease("scene_manager")
            .await
            .context("cannot get wake lease from SAG")?;
        log::info!("InteractionStateHandler created a wake lease during initialization.");

        Ok(Self { activity_governor, wake_lease: Some(wake_lease) })
    }

    async fn create_lease(&mut self) -> Result<(), Error> {
        if self.wake_lease.is_some() {
            log::warn!("InteractionStateHandler already held a wake lease when trying to create one, please investigate.");
            return Ok(());
        }

        let wake_lease = self
            .activity_governor
            .take_wake_lease("scene_manager")
            .await
            .context("cannot get wake lease from SAG")?;
        self.wake_lease = Some(wake_lease);
        log::info!(
            "InteractionStateHandler created a wake lease due to receiving recent user input."
        );

        Ok(())
    }

    fn drop_lease(&mut self) {
        if let Some(lease) = self.wake_lease.take() {
            log::info!("InteractionStateHandler is dropping the wake lease due to not receiving any recent user input.");
            std::mem::drop(lease);
        } else {
            log::warn!("InteractionStateHandler was not holding a wake lease when trying to drop one, please investigate.");
        }
    }

    #[cfg(test)]
    fn is_holding_lease(&self) -> bool {
        self.wake_lease.is_some()
    }
}

pub type NotifyFn = Box<dyn Fn(&State, NotifierWatchStateResponder) -> bool>;
pub type InteractionStatePublisher = Publisher<State, NotifierWatchStateResponder, NotifyFn>;
pub type InteractionStateSubscriber = Subscriber<State, NotifierWatchStateResponder, NotifyFn>;
type InteractionHangingGet = HangingGet<State, NotifierWatchStateResponder, NotifyFn>;

/// An [`InteractionStateHandler`] tracks the state of user input interaction.
pub struct InteractionStateHandler {
    // When `idle_threshold_ms` has transpired since the last user input
    // interaction, the user interaction state will transition from active to idle.
    idle_threshold_ms: zx::MonotonicDuration,

    // The task holding the timer-based idle transition after last user input.
    idle_transition_task: Cell<Option<Task<()>>>,

    // The event time of the last user input interaction.
    last_event_time: RefCell<zx::MonotonicInstant>,

    // To support power management, the caller must provide `Some` value for
    // `lease_holder`. The existence of a `LeaseHolder` implies power framework
    // availability in the platform.
    lease_holder: Option<Rc<RefCell<LeaseHolder>>>,

    // The publisher used to set active/idle state with hanging-get subscribers.
    state_publisher: InteractionStatePublisher,

    /// The inventory of this handler's Inspect status.
    pub inspect_status: InputHandlerStatus,
}

impl InteractionStateHandler {
    /// Creates a new [`InteractionStateHandler`] that listens for user input
    /// input interactions and notifies clients of interaction state changes.
    pub async fn new(
        idle_threshold_ms: zx::MonotonicDuration,
        input_handlers_node: &fuchsia_inspect::Node,
        state_publisher: InteractionStatePublisher,
        suspend_enabled: bool,
    ) -> Rc<Self> {
        log::info!(
            "InteractionStateHandler is initialized with idle_threshold_ms: {:?}",
            idle_threshold_ms.into_millis()
        );

        let inspect_status =
            InputHandlerStatus::new(input_handlers_node, "interaction_state_handler", false);

        let lease_holder = match suspend_enabled {
            true => {
                let activity_governor = connect_to_protocol::<ActivityGovernorMarker>()
                    .expect("connect to fuchsia.power.system.ActivityGovernor");
                match LeaseHolder::new(activity_governor).await {
                    Ok(holder) => Some(Rc::new(RefCell::new(holder))),
                    Err(e) => {
                        log::error!("Unable to integrate with power, system may incorrectly enter suspend: {:?}", e);
                        None
                    }
                }
            }
            false => None,
        };

        Rc::new(Self::new_internal(
            idle_threshold_ms,
            zx::MonotonicInstant::get(),
            lease_holder,
            inspect_status,
            state_publisher,
        ))
    }

    #[cfg(test)]
    /// Sets the initial idleness timer relative to fake time at 0 for tests.
    async fn new_for_test(
        idle_threshold_ms: zx::MonotonicDuration,
        lease_holder: Option<Rc<RefCell<LeaseHolder>>>,
        state_publisher: InteractionStatePublisher,
    ) -> Rc<Self> {
        fuchsia_async::TestExecutor::advance_to(zx::MonotonicInstant::ZERO.into()).await;

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("test_node");
        let inspect_status = InputHandlerStatus::new(
            &test_node,
            "interaction_state_handler",
            /* generates_events */ false,
        );
        Rc::new(Self::new_internal(
            idle_threshold_ms,
            zx::MonotonicInstant::ZERO,
            lease_holder,
            inspect_status,
            state_publisher,
        ))
    }

    fn new_internal(
        idle_threshold_ms: zx::MonotonicDuration,
        initial_timestamp: zx::MonotonicInstant,
        lease_holder: Option<Rc<RefCell<LeaseHolder>>>,
        inspect_status: InputHandlerStatus,
        state_publisher: InteractionStatePublisher,
    ) -> Self {
        let task = Self::create_idle_transition_task(
            initial_timestamp + idle_threshold_ms,
            state_publisher.clone(),
            lease_holder.clone(),
        );

        Self {
            idle_threshold_ms,
            idle_transition_task: Cell::new(Some(task)),
            last_event_time: RefCell::new(initial_timestamp),
            lease_holder,
            state_publisher,
            inspect_status,
        }
    }

    async fn transition_to_active(
        state_publisher: &InteractionStatePublisher,
        lease_holder: &Option<Rc<RefCell<LeaseHolder>>>,
    ) {
        if let Some(holder) = lease_holder {
            if let Err(e) = holder.borrow_mut().create_lease().await {
                log::warn!(
                    "Unable to create lease, system may incorrectly go into suspend: {:?}",
                    e
                );
            };
        }
        state_publisher.set(State::Active);
    }

    fn create_idle_transition_task(
        timeout: zx::MonotonicInstant,
        state_publisher: InteractionStatePublisher,
        lease_holder: Option<Rc<RefCell<LeaseHolder>>>,
    ) -> Task<()> {
        Task::local(async move {
            Timer::new(timeout).await;
            lease_holder.and_then(|holder| Some(holder.borrow_mut().drop_lease()));
            state_publisher.set(State::Idle);
        })
    }

    async fn transition_to_idle_after_new_time(&self, event_time: zx::MonotonicInstant) {
        if *self.last_event_time.borrow() > event_time {
            return;
        }

        *self.last_event_time.borrow_mut() = event_time;
        if let Some(t) = self.idle_transition_task.take() {
            // If the task returns a completed output, we can assume the
            // state has transitioned to Idle.
            if let Some(()) = t.cancel().await {
                Self::transition_to_active(&self.state_publisher, &self.lease_holder).await;
            }
        }

        self.idle_transition_task.set(Some(Self::create_idle_transition_task(
            event_time + self.idle_threshold_ms,
            self.state_publisher.clone(),
            self.lease_holder.clone(),
        )));
    }

    #[cfg(test)]
    fn is_holding_lease(&self) -> bool {
        if let Some(holder) = &self.lease_holder {
            return holder.borrow().is_holding_lease();
        }

        false
    }
}

/// Handles the request stream for fuchsia.input.interaction.Notifier.
///
/// # Parameters
/// `stream`: The `NotifierRequestStream` to be handled.
pub async fn handle_interaction_notifier_request_stream(
    mut stream: NotifierRequestStream,
    subscriber: InteractionStateSubscriber,
) -> Result<(), Error> {
    while let Some(notifier_request) = stream.next().await {
        let NotifierRequest::WatchState { responder } = notifier_request?;
        subscriber.register(responder)?;
    }

    Ok(())
}

pub fn init_interaction_hanging_get() -> InteractionHangingGet {
    let notify_fn: NotifyFn = Box::new(|state, responder| {
        if responder.send(*state).is_err() {
            log::info!("Failed to send user input interaction state");
        }

        true
    });

    let initial_state = State::Active;
    InteractionHangingGet::new(initial_state, notify_fn)
}

#[async_trait(?Send)]
impl UnhandledInputHandler for InteractionStateHandler {
    /// This InputHandler doesn't consume any input events.
    /// It just passes them on to the next handler in the pipeline.
    async fn handle_unhandled_input_event(
        self: Rc<Self>,
        unhandled_input_event: input_device::UnhandledInputEvent,
    ) -> Vec<input_device::InputEvent> {
        match unhandled_input_event.device_event {
            input_device::InputDeviceEvent::ConsumerControls(_)
            | input_device::InputDeviceEvent::Mouse(_)
            | input_device::InputDeviceEvent::TouchScreen(_) => {
                self.inspect_status.count_received_event(input_device::InputEvent::from(
                    unhandled_input_event.clone(),
                ));

                // Clamp the time to now so that clients cannot send events far off
                // in the future to keep the system always active.
                // Note: We use the global executor to get the current time instead
                // of the kernel so that we do not unnecessarily clamp
                // test-injected times.
                let event_time = unhandled_input_event.event_time.clamp(
                    zx::MonotonicInstant::ZERO,
                    fuchsia_async::MonotonicInstant::now().into_zx(),
                );

                self.transition_to_idle_after_new_time(event_time).await;
            }
            _ => {}
        }

        vec![input_device::InputEvent::from(unhandled_input_event)]
    }

    fn set_handler_healthy(self: std::rc::Rc<Self>) {
        self.inspect_status.health_node.borrow_mut().set_ok();
    }

    fn set_handler_unhealthy(self: std::rc::Rc<Self>, msg: &str) {
        self.inspect_status.health_node.borrow_mut().set_unhealthy(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mouse_binding;
    use crate::testing_utilities::{
        consumer_controls_device_descriptor, create_consumer_controls_event, create_mouse_event,
        create_touch_contact, create_touch_screen_event, get_mouse_device_descriptor,
        get_touch_screen_device_descriptor,
    };
    use crate::utils::Position;
    use assert_matches::assert_matches;
    use async_utils::hanging_get::client::HangingGetStream;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_input_interaction::{NotifierMarker, NotifierProxy};
    use fidl_fuchsia_power_system::{ActivityGovernorMarker, ActivityGovernorRequest};
    use fidl_fuchsia_ui_input::PointerEventPhase;
    use fuchsia_async::TestExecutor;
    use futures::pin_mut;
    use maplit::hashmap;
    use std::collections::HashSet;
    use std::task::Poll;
    use test_case::test_case;

    const ACTIVITY_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(5000);

    async fn create_interaction_state_handler_and_notifier_proxy(
        suspend_enabled: bool,
    ) -> (Rc<InteractionStateHandler>, NotifierProxy) {
        let mut interaction_hanging_get = init_interaction_hanging_get();

        let (notifier_proxy, notifier_stream) = create_proxy_and_stream::<NotifierMarker>();
        let stream_fut = handle_interaction_notifier_request_stream(
            notifier_stream,
            interaction_hanging_get.new_subscriber(),
        );

        Task::local(async move {
            if stream_fut.await.is_err() {
                panic!("Failed to handle notifier request stream");
            }
        })
        .detach();

        let lease_holder = match suspend_enabled {
            true => {
                let holder = LeaseHolder::new(fake_activity_governor_server())
                    .await
                    .expect("create lease holder for test");
                Some(Rc::new(RefCell::new(holder)))
            }
            false => None,
        };

        (
            InteractionStateHandler::new_for_test(
                ACTIVITY_TIMEOUT,
                lease_holder,
                interaction_hanging_get.new_publisher(),
            )
            .await,
            notifier_proxy,
        )
    }

    fn fake_activity_governor_server() -> ActivityGovernorProxy {
        let (proxy, mut stream) = create_proxy_and_stream::<ActivityGovernorMarker>();
        Task::local(async move {
            while let Some(request) = stream.next().await {
                match request {
                    Ok(ActivityGovernorRequest::TakeWakeLease { responder, .. }) => {
                        let (_, fake_wake_lease) = zx::EventPair::create();
                        responder.send(fake_wake_lease).expect("failed to send fake wake lease");
                    }
                    Ok(unexpected) => {
                        log::warn!(
                            "Unexpected request {unexpected:?} serving fuchsia.power.system.ActivityGovernor"
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "Error serving fuchsia.power.system.ActivityGovernor: {:?}",
                            e
                        );
                    }
                }
            }
        })
        .detach();

        proxy
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test(allow_stalls = false)]
    async fn notifier_sends_initial_state(suspend_enabled: bool) {
        let (interaction_state_handler, notifier_proxy) =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled).await;
        let state = notifier_proxy.watch_state().await.expect("Failed to get interaction state");
        assert_eq!(state, State::Active);
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test]
    fn notifier_sends_idle_state_after_timeout(suspend_enabled: bool) -> Result<(), Error> {
        let mut executor = TestExecutor::new_with_fake_time();

        let handler_and_proxy_fut =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled);
        pin_mut!(handler_and_proxy_fut);
        let handler_and_proxy_res = executor.run_until_stalled(&mut handler_and_proxy_fut);
        let (interaction_state_handler, notifier_proxy) = match handler_and_proxy_res {
            Poll::Ready((handler, proxy)) => (handler, proxy),
            _ => panic!("Unable to create interaction state handler and proxy"),
        };

        // Initial state is active.
        let mut watch_state_stream =
            HangingGetStream::new(notifier_proxy, NotifierProxy::watch_state);
        let state_fut = watch_state_stream.next();
        pin_mut!(state_fut);
        let initial_state = executor.run_until_stalled(&mut state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT));

        // State transitions to Idle.
        let idle_state_fut = watch_state_stream.next();
        pin_mut!(idle_state_fut);
        let initial_state = executor.run_until_stalled(&mut idle_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Idle))));
        assert_eq!(interaction_state_handler.is_holding_lease(), false);

        Ok(())
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test]
    fn interaction_state_handler_drops_first_timer_on_activity(
        suspend_enabled: bool,
    ) -> Result<(), Error> {
        // This test does the following:
        //   - Start an InteractionStateHandler, whose initial timeout is set to
        //     ACTIVITY_TIMEOUT.
        //   - Send an activity at time ACTIVITY_TIMEOUT / 2.
        //   - Observe that after ACTIVITY_TIMEOUT transpires, the initial
        //     timeout to transition to idle state _does not_ fire, as we
        //     expect it to be replaced by a new timeout in response to the
        //     injected activity.
        //   - Observe that after ACTIVITY_TIMEOUT * 1.5 transpires, the second
        //     timeout to transition to idle state _does_ fire.
        // Because division will round to 0, odd-number timeouts could cause an
        // incorrect implementation to still pass the test. In order to catch
        // these cases, we first assert that ACTIVITY_TIMEOUT is an even number.
        assert_eq!(ACTIVITY_TIMEOUT.into_nanos() % 2, 0);

        let mut executor = TestExecutor::new_with_fake_time();

        let handler_and_proxy_fut =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled);
        pin_mut!(handler_and_proxy_fut);
        let handler_and_proxy_res = executor.run_until_stalled(&mut handler_and_proxy_fut);
        let (interaction_state_handler, notifier_proxy) = match handler_and_proxy_res {
            Poll::Ready((handler, proxy)) => (handler, proxy),
            _ => panic!("Unable to create interaction state handler and proxy"),
        };

        // Initial state is active.
        let mut watch_state_stream =
            HangingGetStream::new(notifier_proxy, NotifierProxy::watch_state);
        let state_fut = watch_state_stream.next();
        pin_mut!(state_fut);
        let initial_state = executor.run_until_stalled(&mut state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by half the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT / 2));

        // Send an input event, replacing the initial idleness timer.
        let input_event =
            input_device::UnhandledInputEvent::try_from(create_consumer_controls_event(
                vec![fidl_fuchsia_input_report::ConsumerControlButton::Power],
                zx::MonotonicInstant::from(fuchsia_async::MonotonicInstant::after(
                    ACTIVITY_TIMEOUT / 2,
                )),
                &consumer_controls_device_descriptor(),
            ))
            .unwrap();

        let mut handle_event_fut =
            interaction_state_handler.clone().handle_unhandled_input_event(input_event);
        let handle_result = executor.run_until_stalled(&mut handle_event_fut);
        assert!(handle_result.is_ready());

        // Skip ahead by half the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT / 2));

        // Initial state does not change.
        let watch_state_fut = watch_state_stream.next();
        pin_mut!(watch_state_fut);
        let watch_state_res = executor.run_until_stalled(&mut watch_state_fut);
        assert_matches!(watch_state_res, Poll::Pending);
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by half the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT / 2));

        // Interaction state does change.
        let watch_state_res = executor.run_until_stalled(&mut watch_state_fut);
        assert_matches!(watch_state_res, Poll::Ready(Some(Ok(State::Idle))));
        assert_eq!(interaction_state_handler.is_holding_lease(), false);

        Ok(())
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test]
    fn interaction_state_handler_drops_late_activities(suspend_enabled: bool) -> Result<(), Error> {
        let mut executor = TestExecutor::new_with_fake_time();

        let handler_and_proxy_fut =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled);
        pin_mut!(handler_and_proxy_fut);
        let handler_and_proxy_res = executor.run_until_stalled(&mut handler_and_proxy_fut);
        let (interaction_state_handler, notifier_proxy) = match handler_and_proxy_res {
            Poll::Ready((handler, proxy)) => (handler, proxy),
            _ => panic!("Unable to create interaction state handler and proxy"),
        };

        // Initial state is active.
        let mut watch_state_stream =
            HangingGetStream::new(notifier_proxy, NotifierProxy::watch_state);
        let state_fut = watch_state_stream.next();
        pin_mut!(state_fut);
        let watch_state_res = executor.run_until_stalled(&mut state_fut);
        assert_matches!(watch_state_res, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by half the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT / 2));

        // Send an input event, replacing the initial idleness timer.
        let input_event =
            input_device::UnhandledInputEvent::try_from(create_consumer_controls_event(
                vec![fidl_fuchsia_input_report::ConsumerControlButton::Power],
                zx::MonotonicInstant::from(fuchsia_async::MonotonicInstant::after(
                    ACTIVITY_TIMEOUT / 2,
                )),
                &consumer_controls_device_descriptor(),
            ))
            .unwrap();

        let mut handle_event_fut =
            interaction_state_handler.clone().handle_unhandled_input_event(input_event);
        let handle_result = executor.run_until_stalled(&mut handle_event_fut);
        assert!(handle_result.is_ready());

        // Skip ahead by half the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT / 2));

        // Send an input event with an earlier event time.
        let input_event =
            input_device::UnhandledInputEvent::try_from(create_consumer_controls_event(
                vec![fidl_fuchsia_input_report::ConsumerControlButton::Power],
                zx::MonotonicInstant::ZERO,
                &consumer_controls_device_descriptor(),
            ))
            .unwrap();

        let mut handle_event_fut =
            interaction_state_handler.clone().handle_unhandled_input_event(input_event);
        let handle_result = executor.run_until_stalled(&mut handle_event_fut);
        assert!(handle_result.is_ready());

        // Initial task does not transition to idle, nor does one from the
        // "earlier" activity that was received later.
        let watch_state_fut = watch_state_stream.next();
        pin_mut!(watch_state_fut);
        let initial_state = executor.run_until_stalled(&mut watch_state_fut);
        assert_matches!(initial_state, Poll::Pending);
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by half the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT / 2));

        // Interaction state does change.
        let watch_state_res = executor.run_until_stalled(&mut watch_state_fut);
        assert_matches!(watch_state_res, Poll::Ready(Some(Ok(State::Idle))));
        assert_eq!(interaction_state_handler.is_holding_lease(), false);

        Ok(())
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test]
    fn notifier_sends_active_state_with_button_input_event(
        suspend_enabled: bool,
    ) -> Result<(), Error> {
        let mut executor = TestExecutor::new_with_fake_time();

        let handler_and_proxy_fut =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled);
        pin_mut!(handler_and_proxy_fut);
        let handler_and_proxy_res = executor.run_until_stalled(&mut handler_and_proxy_fut);
        let (interaction_state_handler, notifier_proxy) = match handler_and_proxy_res {
            Poll::Ready((handler, proxy)) => (handler, proxy),
            _ => panic!("Unable to create interaction state handler and proxy"),
        };

        // Initial state is active.
        let mut watch_state_stream =
            HangingGetStream::new(notifier_proxy, NotifierProxy::watch_state);
        let state_fut = watch_state_stream.next();
        pin_mut!(state_fut);
        let initial_state = executor.run_until_stalled(&mut state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT));

        // State transitions to Idle.
        let idle_state_fut = watch_state_stream.next();
        pin_mut!(idle_state_fut);
        let initial_state = executor.run_until_stalled(&mut idle_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Idle))));
        assert_eq!(interaction_state_handler.is_holding_lease(), false);

        // Send an input event.
        let input_event =
            input_device::UnhandledInputEvent::try_from(create_consumer_controls_event(
                vec![fidl_fuchsia_input_report::ConsumerControlButton::Power],
                zx::MonotonicInstant::get(),
                &consumer_controls_device_descriptor(),
            ))
            .unwrap();

        let mut handle_event_fut =
            interaction_state_handler.clone().handle_unhandled_input_event(input_event);
        let handle_result = executor.run_until_stalled(&mut handle_event_fut);

        // Event is not handled.
        match handle_result {
            Poll::Ready(res) => assert_matches!(
                res.as_slice(),
                [input_device::InputEvent { handled: input_device::Handled::No, .. }]
            ),
            x => panic!("expected Ready from handle_unhandled_input_event, got {:?}", x),
        };

        // State transitions to Active.
        let active_state_fut = watch_state_stream.next();
        pin_mut!(active_state_fut);
        let initial_state = executor.run_until_stalled(&mut active_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        Ok(())
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test]
    fn notifier_sends_active_state_with_mouse_input_event(
        suspend_enabled: bool,
    ) -> Result<(), Error> {
        let mut executor = TestExecutor::new_with_fake_time();

        let handler_and_proxy_fut =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled);
        pin_mut!(handler_and_proxy_fut);
        let handler_and_proxy_res = executor.run_until_stalled(&mut handler_and_proxy_fut);
        let (interaction_state_handler, notifier_proxy) = match handler_and_proxy_res {
            Poll::Ready((handler, proxy)) => (handler, proxy),
            _ => panic!("Unable to create interaction state handler and proxy"),
        };

        // Initial state is active.
        let mut watch_state_stream =
            HangingGetStream::new(notifier_proxy, NotifierProxy::watch_state);
        let state_fut = watch_state_stream.next();
        pin_mut!(state_fut);
        let initial_state = executor.run_until_stalled(&mut state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT));

        // State transitions to Idle.
        let idle_state_fut = watch_state_stream.next();
        pin_mut!(idle_state_fut);
        let initial_state = executor.run_until_stalled(&mut idle_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Idle))));
        assert_eq!(interaction_state_handler.is_holding_lease(), false);

        // Send an input event.
        let input_event = input_device::UnhandledInputEvent::try_from(create_mouse_event(
            mouse_binding::MouseLocation::Absolute(Position { x: 0.0, y: 0.0 }),
            None, /* wheel_delta_v */
            None, /* wheel_delta_h */
            None, /* is_precision_scroll */
            mouse_binding::MousePhase::Down,
            HashSet::new(),
            HashSet::new(),
            zx::MonotonicInstant::get(),
            &get_mouse_device_descriptor(),
        ))
        .unwrap();

        let mut handle_event_fut =
            interaction_state_handler.clone().handle_unhandled_input_event(input_event);
        let handle_result = executor.run_until_stalled(&mut handle_event_fut);

        // Event is not handled.
        match handle_result {
            Poll::Ready(res) => assert_matches!(
                res.as_slice(),
                [input_device::InputEvent { handled: input_device::Handled::No, .. }]
            ),
            x => panic!("expected Ready from handle_unhandled_input_event, got {:?}", x),
        };

        // State transitions to Active.
        let active_state_fut = watch_state_stream.next();
        pin_mut!(active_state_fut);
        let initial_state = executor.run_until_stalled(&mut active_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        Ok(())
    }

    #[test_case(true; "Suspend enabled")]
    #[test_case(false; "Suspend disabled")]
    #[fuchsia::test]
    fn notifier_sends_active_state_with_touch_input_event(
        suspend_enabled: bool,
    ) -> Result<(), Error> {
        let mut executor = TestExecutor::new_with_fake_time();

        let handler_and_proxy_fut =
            create_interaction_state_handler_and_notifier_proxy(suspend_enabled);
        pin_mut!(handler_and_proxy_fut);
        let handler_and_proxy_res = executor.run_until_stalled(&mut handler_and_proxy_fut);
        let (interaction_state_handler, notifier_proxy) = match handler_and_proxy_res {
            Poll::Ready((handler, proxy)) => (handler, proxy),
            _ => panic!("Unable to create interaction state handler and proxy"),
        };

        // Initial state is active.
        let mut watch_state_stream =
            HangingGetStream::new(notifier_proxy, NotifierProxy::watch_state);
        let state_fut = watch_state_stream.next();
        pin_mut!(state_fut);
        let initial_state = executor.run_until_stalled(&mut state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        // Skip ahead by the activity timeout.
        executor.set_fake_time(fuchsia_async::MonotonicInstant::after(ACTIVITY_TIMEOUT));

        // State transitions to Idle.
        let idle_state_fut = watch_state_stream.next();
        pin_mut!(idle_state_fut);
        let initial_state = executor.run_until_stalled(&mut idle_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Idle))));
        assert_eq!(interaction_state_handler.is_holding_lease(), false);

        // Send an input event.
        const TOUCH_ID: u32 = 1;
        let contact = create_touch_contact(TOUCH_ID, Position { x: 0.0, y: 0.0 });
        let input_event = input_device::UnhandledInputEvent::try_from(create_touch_screen_event(
            hashmap! {
                PointerEventPhase::Add
                    => vec![contact.clone()],
            },
            zx::MonotonicInstant::get(),
            &get_touch_screen_device_descriptor(),
        ))
        .unwrap();

        let mut handle_event_fut =
            interaction_state_handler.clone().handle_unhandled_input_event(input_event);
        let handle_result = executor.run_until_stalled(&mut handle_event_fut);

        // Event is not handled.
        match handle_result {
            Poll::Ready(res) => assert_matches!(
                res.as_slice(),
                [input_device::InputEvent { handled: input_device::Handled::No, .. }]
            ),
            x => panic!("expected Ready from handle_unhandled_input_event, got {:?}", x),
        };

        // State transitions to Active.
        let active_state_fut = watch_state_stream.next();
        pin_mut!(active_state_fut);
        let initial_state = executor.run_until_stalled(&mut active_state_fut);
        assert_matches!(initial_state, Poll::Ready(Some(Ok(State::Active))));
        assert_eq!(interaction_state_handler.is_holding_lease(), suspend_enabled);

        Ok(())
    }
}
