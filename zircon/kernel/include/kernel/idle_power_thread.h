// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_H_

#include <lib/lazy_init/lazy_init.h>
#include <lib/relaxed_atomic.h>
#include <lib/wake-vector.h>
#include <stdint.h>
#include <zircon/time.h>

#include <arch/mp_unplug_event.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <kernel/thread.h>
#include <kernel/timer.h>

//
// Platform Independent Per-CPU Idle/Power Thread
//
// Manages the idle, suspend, and offline functions of a CPU. This thread is scheduled whenever
// there is no eligible work in the run queues of a CPU or a power state management operation is
// requested for the CPU. The idle/power thread provides the platform-independent run loop for
// handling these functions and delegates to the platform-specific subsystems for the relevant
// operations.
//

class IdlePowerThread final {
 public:
  IdlePowerThread() = default;
  ~IdlePowerThread() = default;

  IdlePowerThread(const IdlePowerThread&) = delete;
  IdlePowerThread& operator=(const IdlePowerThread&) = delete;
  IdlePowerThread(IdlePowerThread&&) = delete;
  IdlePowerThread& operator=(IdlePowerThread&&) = delete;

  // The current or target high-level operational state of the CPU. The depth and type of
  // idle/suspend states depends inputs from other subsystems, such as timer and run queues and
  // system power targets.
  // TODO(https://fxbug.dev/42086705): Rationalize this state with mp active / online states.
  enum class State : uint8_t {
    Active,
    Wakeup,  // Similar to Active with respect to scheduling but signals a wakeup occurred.
    Offline,
    Suspend,
  };

  // Returns true if the power thread should be scheduled instead of eligible work because it needs
  // to perform a transition or power management function.
  bool pending_power_work() const {
    const StateMachine state = state_.load(ktl::memory_order_acquire);
    return (state.current != State::Active && state.current != State::Wakeup) ||
           state.current != state.target;
  }

  // The result of a transition request, including the status code of the request and the current
  // state when the request was made.
  struct TransitionResult {
    const zx_status_t status;
    const State starting_state;
  };

  // Requests to transition from Active to Offline.
  //
  // Returns:
  //   - {ZX_OK, Active} if the transition was successful.
  //   - {ZX_OK, Offline} if the current state was already Offline.
  //   - {ZX_ERR_BAD_STATE, current state} if the current state was not Active or Offline.
  //   - {ZX_ERR_TIMED_OUT, Active} if the current state was Active and transition timeout expired.
  TransitionResult TransitionActiveToOffline(zx_instant_mono_t timeout_at = ZX_TIME_INFINITE)
      TA_EXCL(TransitionLock::Get()) {
    Guard<Mutex> guard{TransitionLock::Get()};
    return TransitionFromTo(State::Active, State::Offline, timeout_at);
  }

  // Requests to transition from Offline to Active.
  //
  // Returns:
  //   - {ZX_OK, Offline} if the transition was successful.
  //   - {ZX_OK, Active} if the current state was already Active.
  //   - {ZX_ERR_BAD_STATE, current state} if the current state was not Active or Offline.
  //   - {ZX_ERR_TIMED_OUT, Offline} if the current state was Offline and transition timeout
  //     expired.
  TransitionResult TransitionOfflineToActive(zx_instant_mono_t timeout_at = ZX_TIME_INFINITE)
      TA_EXCL(TransitionLock::Get()) {
    Guard<Mutex> guard{TransitionLock::Get()};
    return TransitionFromTo(State::Offline, State::Active, timeout_at);
  }

  // Attempts to transition all active CPUs from Active to Suspend until the specified resume time.
  // CPUs that are Offline before suspend will remain Offline after resume until explicitly
  // transitioned to Active.
  //
  // Returns:
  //  - ZX_OK if active CPU suspend succeeded and then resumed.
  //  - ZX_ERR_TIMED_OUT when resume_at is in the past before transitioning to suspend.
  //  - ZX_ERR_BAD_STATE if there is a pending wake event when attempting to transition to suspend.
  static zx_status_t TransitionAllActiveToSuspend(zx_instant_boot_t resume_at = ZX_TIME_INFINITE)
      TA_EXCL(TransitionLock::Get());

  // Implements the run loop executed by the CPU's idle/power thread.
  static int Run(void* arg);

  static void InitHook(uint level);

  // Called by mp_unplug_current_cpu() to complete last steps of taking the CPU offline.
  void FlushAndHalt();

  // Accessors to the underlying Thread instance.
  Thread& thread() { return thread_; }
  const Thread& thread() const { return thread_; }

  // Accessor to the global system suspend state.
  static bool system_suspended() {
    return system_suspend_state_.load(ktl::memory_order_acquire) & SystemSuspendStateSuspendedBit;
  }
  static uint64_t pending_wake_events() {
    return system_suspend_state_.load(ktl::memory_order_acquire) &
           SystemSuspendStateWakeEventPendingMask;
  }

 private:
  // Allow percpu to initialize this_cpu_;
  friend struct percpu;

  // Allow Scheduler to get/reset the time spent in the processor idle state.
  friend class Scheduler;

  struct StateMachine {
    State current{State::Active};
    State target{State::Active};

    constexpr bool operator==(const StateMachine& other) const {
      return current == other.current && target == other.target;
    }
  };

  // Shorthand expressions for various comparisons and asserts.
  static constexpr StateMachine kActive{State::Active, State::Active};
  static constexpr StateMachine kActiveToSuspend{State::Active, State::Suspend};
  static constexpr StateMachine kSuspendToActive{State::Suspend, State::Active};
  static constexpr StateMachine kWakeup{State::Wakeup, State::Wakeup};
  static constexpr StateMachine kSuspend{State::Suspend, State::Suspend};
  static constexpr StateMachine kSuspendToWakeup{State::Suspend, State::Wakeup};

  bool CompareExchangeState(StateMachine& expected, StateMachine desired) {
    return state_.compare_exchange_strong(expected, desired, ktl::memory_order_acq_rel,
                                          ktl::memory_order_acquire);
  }
  TransitionResult TransitionFromTo(State expected_state, State target_state,
                                    zx_instant_mono_t timeout_at) TA_REQ(TransitionLock::Get());

  using WakeResult = wake_vector::WakeResult;
  using WakeEvent = wake_vector::WakeEvent;

  friend WakeEvent;

  static WakeResult WakeBootCpu();
  static WakeResult TriggerSystemWakeEvent();
  static void AcknowledgeSystemWakeEvent();

  // This function does different things depending on the current_cpu and current_state.
  //
  // 1. If the current_cpu is equal to the boot CPU, and:
  //    a. If the state indicates that we are suspended, then pause the monotonic clock.
  //    b. If the state indicates that we are waking up, then unpause the monotonic clock and update
  //       the platform timer.
  // 2. If the current_cpu is a secondary CPU and the state indicates that we are active, then
  //    just update the platform timer.
  //
  // The boot CPU is responsible for pausing and resuming the clock since it is the last CPU to
  // suspend and the first CPU to resume. However, each CPU has its own platform timer that it must
  // update after the monotonic clock is resumed.
  static void UpdateMonotonicClock(cpu_num_t current_cpu, const StateMachine& current_state);

  // Called by the scheduler when updating per-CPU and per-thread stats.
  static zx_duration_mono_t TakeProcessorIdleTime();

  ktl::atomic<StateMachine> state_;
  AutounsignalMpUnplugEvent complete_;

  Thread thread_;
  cpu_num_t this_cpu_{INVALID_CPU};

  // Accumulates the time spent in a lower-power idle state since the last reschedule. This value is
  // reset to zero on each reschedule and when a CPU goes offline.
  zx_duration_mono_t processor_idle_time_ns_{0};

  DECLARE_SINGLETON_MUTEX(TransitionLock);

  enum SystemSuspendState : uint64_t {
    SystemSuspendStateActive = 0,
    SystemSuspendStateSuspendedBit = uint64_t{1} << 63,
    SystemSuspendStateWakeEventPendingMask = SystemSuspendStateSuspendedBit - 1,
  };

  // Provides an interlock between pending wake events and system suspend, preventing suspend entry
  // until there are no pending wake events.
  inline static ktl::atomic<uint64_t> system_suspend_state_{SystemSuspendStateActive};

  // A timer on the boot timeline used to resume the system at the given boot time if no other wake
  // events occur by then.
  inline static Timer resume_timer_ TA_GUARDED(TransitionLock::Get()){ZX_CLOCK_BOOT};

  // A bespoke wake vector used by the resume timer handler to wake the system when the resume boot
  // time is reached.
  struct ResumeTimerWakeVector : public wake_vector::WakeVector {
    ResumeTimerWakeVector() : WakeVector{&ResumeTimerWakeVector::wake_event}, wake_event{*this} {
      wake_event.Initialize();
    }
    ~ResumeTimerWakeVector() { wake_event.Destroy(); }

    void GetDiagnostics(WakeVector::Diagnostics& diagnostics) const final;

    WakeEvent wake_event;
  };

  // Use LazyInit to sequence constructing this instance after global ctors initialize the global
  // WakeEvent list. This instance is initialized by an init hook well before it can be used at
  // runtime by TransitionAllActiveToSuspend.
  inline static lazy_init::LazyInit<ResumeTimerWakeVector> resume_timer_wake_vector_;
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_IDLE_POWER_THREAD_H_
