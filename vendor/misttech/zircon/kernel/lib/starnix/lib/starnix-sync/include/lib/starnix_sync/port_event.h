// Copyright 2024 Mist Tecnologia LTDA. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef VENDOR_MISTTECH_ZIRCON_KERNEL_LIB_STARNIX_LIB_STARNIX_SYNC_INCLUDE_LIB_STARNIX_SYNC_PORT_EVENT_H_
#define VENDOR_MISTTECH_ZIRCON_KERNEL_LIB_STARNIX_LIB_STARNIX_SYNC_INCLUDE_LIB_STARNIX_SYNC_PORT_EVENT_H_

#include <lib/fit/result.h>
#include <zircon/syscalls/port.h>
#include <zircon/types.h>

#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/event.h>
#include <ktl/atomic.h>
#include <ktl/utility.h>
#include <ktl/variant.h>
#include <object/dispatcher.h>
#include <object/port_dispatcher.h>

namespace starnix_sync {

// A PortEvent is interested only in events originating from within the
// process (see PortEvent::futex_ for more details), and the waiter
// may be notified.
constexpr int32_t FUTEX_WAITING = 0;

// A PortEvent is interested only in events originating from within the
// process (see PortEvent::futex_ for more details), and the waiter
// has been notified of an event.
constexpr int32_t FUTEX_NOTIFIED = 1;

// A PortEvent is interested only in events originating from within the
// process (see PortEvent::futex_ for more details), and the waiter
// has been notified of an interrupt.
constexpr int32_t FUTEX_INTERRUPTED = 2;

// A PortEvent is interested in events originating from outside of
// process (see PortEvent::futex_ for more details). The waiter's zx::Port
// should be used instead of the Futex.
constexpr int32_t FUTEX_USE_PORT = 3;

// Specifies the ordering for atomics accessed by both the "notifier" and
// "notifee" (the waiter).
//
// Relaxed ordering because the PortEvent does not provide synchronization
// between the "notifier" and the "notifee". If a notifee needs synchronization,
// it needs to perform that synchronization itself.
//
// See PortEvent::Wait for more details.
constexpr std::memory_order ORDERING_FOR_ATOMICS_BETWEEN_NOTIFIER_AND_NOTIFEE =
    std::memory_order_relaxed;

// The kind of notification.
enum class NotifyKind : uint8_t { Regular, Interrupt };

// The result of a call to PortEvent::Wait.
class PortWaitResult {
 public:
  /// Signals asserted on an object.
  struct Signal {
    uint64_t key;
    zx_signals_t observed;
  };

  /// A notification to wake up waiters.
  struct Notification {
    NotifyKind kind;
  };

  struct TimedOut {};

  using Variant = ktl::variant<Signal, Notification, TimedOut>;

  const Variant& result() const { return result_; }

  static PortWaitResult SignalResult(uint64_t key, zx_signals_t observed) {
    return PortWaitResult(Signal{.key = key, .observed = observed});
  }
  static PortWaitResult NotificationResult(NotifyKind kind) {
    return PortWaitResult(Notification{.kind = kind});
  }
  static PortWaitResult TimedOutResult() { return PortWaitResult(TimedOut{}); }

  static PortWaitResult NotifyRegular() {
    return PortWaitResult(Notification{.kind = NotifyKind::Regular});
  }
  static PortWaitResult NotifyInterrupt() {
    return PortWaitResult(Notification{.kind = NotifyKind::Interrupt});
  }

  // Helpers from the reference documentation for ktl::visit<>, to allow
  // visit-by-overload of the ktl::variant<> returned by GetLastReference():
  template <class... Ts>
  struct overloaded : Ts... {
    using Ts::operator()...;
  };
  // explicit deduction guide (not needed as of C++20)
  template <class... Ts>
  overloaded(Ts...) -> overloaded<Ts...>;

 private:
  explicit PortWaitResult(Variant result) : result_(result) {}

  Variant result_;
};

/// A wrapper around a [`zx::Port`] that optimizes for the case where events are
/// signaled within a process.
///
/// This object will prefer to use a Futex for notifications/interrupts but will
/// fallback to a `zx::Port` when the port is subscribed for events on an object
/// with [`PortEvent.object_wait_async`].
///
/// Note that the `PortEvent` does not provide any synchronization between a
/// notifier (caller of [`PortEvent.notify`]) and a notifiee/waiter (caller of
/// [`PortEvent.wait`].
class PortEvent : public fbl::RefCounted<PortEvent> {
 public:
  PortEvent();

  /// Wait for an event to occur, or the deadline has been reached.
  PortWaitResult Wait(zx_instant_mono_t deadline);

  /// Subscribe for signals on an object.
  fit::result<zx_status_t> ObjectWaitAsync(fbl::RefPtr<Dispatcher> handle, uint64_t key,
                                           zx_signals_t signals, uint32_t opts);

  /// Cancels async port notifications on an object.
  void Cancel(fbl::RefPtr<Dispatcher> handle, uint64_t key);

 private:
  /// Queue a packet to the underlying Zircon port, which will cause the
  /// waiter to wake up.
  ///
  /// This method should only be called when the waiter is interested in
  /// events that may originate from outside of the process.
  void QueueUserPacketData(NotifyKind kind);

 public:
  /// Marks the port as ready to handle a notification (or an interrupt) and
  /// wakes up any blocked waiters.
  void Notify(NotifyKind kind);

 private:
  ktl::atomic<uint8_t> state_;

  /// The Futex used to wake up a thread when this waiter is waiting for
  /// events that don't depend on a `zx::Port`.
  Event event_;

  // The underlying Zircon port that the waiter waits on when it is
  // interested in events that cross process boundaries.
  //
  // Lazily allocated to optimize for the case where waiters are interested
  // only in events triggered within a process.
  KernelHandle<PortDispatcher> port_;
  zx_rights_t rights_;

  // Indicates whether a user packet is sitting in the zx_port_t to wake up
  // waiter after handling user events.
  ktl::atomic<bool> has_pending_user_packet_{false};
};

}  // namespace starnix_sync

#endif  // VENDOR_MISTTECH_ZIRCON_KERNEL_LIB_STARNIX_LIB_STARNIX_SYNC_INCLUDE_LIB_STARNIX_SYNC_PORT_EVENT_H_
