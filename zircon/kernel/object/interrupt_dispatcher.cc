// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/interrupt_dispatcher.h"

#include <platform.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/port.h>

#include <dev/interrupt.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/idle_power_thread.h>
#include <object/port_dispatcher.h>
#include <object/process_dispatcher.h>

InterruptDispatcher::InterruptDispatcher(Flags flags, uint32_t options)
    : WakeVector(&InterruptDispatcher::wake_event_),
      timestamp_(0),
      flags_(flags),
      options_(options),
      state_(InterruptState::IDLE),
      wake_event_(*this) {
  DEBUG_ASSERT((flags & INTERRUPT_UNMASK_PREWAIT) == 0 ||
               (flags & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) == 0);
}

zx_info_interrupt_t InterruptDispatcher::GetInfo() const { return {.options = options_}; }

zx_status_t InterruptDispatcher::WaitForInterrupt(zx_time_t* out_timestamp) {
  while (true) {
    const ktl::optional<zx_status_t> opt_status = BeginWaitForInterrupt(out_timestamp);
    if (opt_status.has_value()) {
      return opt_status.value();
    }

    const zx_status_t block_status = DoWaitForInterruptBlock();
    if (block_status != ZX_OK) {
      return block_status;
    }
  }
}

ktl::optional<zx_status_t> InterruptDispatcher::BeginWaitForInterrupt(zx_time_t* out_timestamp) {
  bool defer_unmask = false;

  {
    Guard<SpinLock, IrqSave> guard{&spinlock_};
    if (port_dispatcher_) {
      return ZX_ERR_BAD_STATE;
    }
    switch (state_) {
      case InterruptState::DESTROYED:
        return ZX_ERR_CANCELED;

      case InterruptState::TRIGGERED:
        state_ = InterruptState::NEEDACK;
        *out_timestamp = timestamp_;
        timestamp_ = 0;
        return event_.Unsignal();

      case InterruptState::NEEDACK:
        if (is_wake_vector()) {
          wake_event_.Acknowledge();
        }
        if (flags_ & INTERRUPT_UNMASK_PREWAIT) {
          UnmaskInterrupt();
        } else if (flags_ & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) {
          defer_unmask = true;
        }
        break;

      case InterruptState::IDLE:
        break;

      default:
        return ZX_ERR_BAD_STATE;
    }
    state_ = InterruptState::WAITING;
  }

  if (defer_unmask) {
    UnmaskInterrupt();
  }

  return ktl::nullopt;
}

zx_status_t InterruptDispatcher::DoWaitForInterruptBlock() {
  ThreadDispatcher::AutoBlocked by(ThreadDispatcher::Blocked::INTERRUPT);
  zx_status_t status = event_.Wait(Deadline::infinite());
  if (status != ZX_OK) {
    // The Event::Wait call was interrupted and we need to retry
    // but before we retry we will set the interrupt state
    // back to IDLE if we are still in the WAITING state.
    Guard<SpinLock, IrqSave> guard{&spinlock_};
    if (state_ == InterruptState::WAITING) {
      state_ = InterruptState::IDLE;
    }
  }
  return status;
}

bool InterruptDispatcher::SendPacketLocked(zx_time_t timestamp) {
  bool status = port_dispatcher_->QueueInterruptPacket(&port_packet_, timestamp);
  if (flags_ & INTERRUPT_MASK_POSTWAIT) {
    MaskInterrupt();
  }
  timestamp_ = 0;
  return status;
}

zx_status_t InterruptDispatcher::Trigger(zx_time_t timestamp) {
  if (!(flags_ & INTERRUPT_VIRTUAL))
    return ZX_ERR_BAD_STATE;

  // Use preempt disable for correctness to prevent rescheduling when waking a
  // thread while holding the spinlock.
  AutoPreemptDisabler preempt_disable;
  Guard<SpinLock, IrqSave> guard{&spinlock_};

  // only record timestamp if this is the first signal since we started waiting
  if (!timestamp_) {
    timestamp_ = timestamp;
  }
  if (state_ == InterruptState::DESTROYED) {
    return ZX_ERR_CANCELED;
  }
  if (state_ == InterruptState::NEEDACK && port_dispatcher_) {
    // Cannot trigger a interrupt without ACK
    // only record timestamp if this is the first signal since we started waiting
    return ZX_OK;
  }

  if (is_wake_vector()) {
    wake_event_.Trigger();
  }

  if (port_dispatcher_) {
    SendPacketLocked(timestamp);
    state_ = InterruptState::NEEDACK;
  } else {
    Signal();
    state_ = InterruptState::TRIGGERED;
  }
  return ZX_OK;
}

void InterruptDispatcher::InterruptHandler() {
  // Using preempt disable is not necessary for correctness, since we should
  // be in an interrupt context with preemption disabled, but we re-disable anyway
  // for clarity and robustness.
  AutoPreemptDisabler preempt_disable;
  Guard<SpinLock, IrqSave> guard{&spinlock_};

  // only record timestamp if this is the first IRQ since we started waiting
  if (!timestamp_) {
    if (flags_ & INTERRUPT_TIMESTAMP_MONO) {
      timestamp_ = current_mono_time();
    } else {
      timestamp_ = current_boot_time();
    }
  }
  if (state_ == InterruptState::NEEDACK && port_dispatcher_) {
    return;
  }
  if (port_dispatcher_) {
    SendPacketLocked(timestamp_);
    state_ = InterruptState::NEEDACK;
  } else {
    if (flags_ & INTERRUPT_MASK_POSTWAIT) {
      MaskInterrupt();
    }
    Signal();
    state_ = InterruptState::TRIGGERED;
  }
  if (is_wake_vector() &&
      (state_ == InterruptState::TRIGGERED || state_ == InterruptState::NEEDACK)) {
    // Trigger the wake event which will wake the system if suspended and prevent entering suspend
    // until acknowledged.
    wake_event_.Trigger();
  }
}

zx_status_t InterruptDispatcher::Destroy() {
  // The interrupt may presently have been fired and we could already be about to acquire the
  // spinlock_ in InterruptHandler. If we were to call UnregisterInterruptHandler whilst holding
  // the spinlock_ then we risk a deadlock scenario where the platform interrupt code may have
  // taken a lock to call InterruptHandler, and it might take the same lock when we call
  // UnregisterInterruptHandler.
  MaskInterrupt();
  DeactivateInterrupt();
  UnregisterInterruptHandler();

  // Use preempt disable for correctness to prevent rescheduling when waking a
  // thread while holding the spinlock.
  AutoPreemptDisabler preempt_disable;
  Guard<SpinLock, IrqSave> guard{&spinlock_};

  if (port_dispatcher_) {
    bool packet_was_in_queue = port_dispatcher_->RemoveInterruptPacket(&port_packet_);
    if ((state_ == InterruptState::NEEDACK) && !packet_was_in_queue) {
      state_ = InterruptState::DESTROYED;
      return ZX_ERR_NOT_FOUND;
    }
    if ((state_ == InterruptState::IDLE) ||
        ((state_ == InterruptState::NEEDACK) && packet_was_in_queue)) {
      state_ = InterruptState::DESTROYED;
      return ZX_OK;
    }
  } else {
    state_ = InterruptState::DESTROYED;
    Signal();
  }
  return ZX_OK;
}

zx_status_t InterruptDispatcher::Bind(fbl::RefPtr<PortDispatcher> port_dispatcher, uint64_t key) {
  AutoPreemptDisabler preempt_disable;
  Guard<SpinLock, IrqSave> guard{&spinlock_};
  if (state_ == InterruptState::DESTROYED) {
    return ZX_ERR_CANCELED;
  }
  if (state_ == InterruptState::WAITING) {
    return ZX_ERR_BAD_STATE;
  }
  if (port_dispatcher_) {
    return ZX_ERR_ALREADY_BOUND;
  }

  // If an interrupt is bound to a port there is a conflict between UNMASK_PREWAIT_UNLOCKED
  // and MASK_POSTWAIT because the mask operation will by necessity happen before leaving the
  // dispatcher spinlock, leading to a mask operation immediately followed by the deferred
  // unmask operation.
  if ((flags_ & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) && (flags_ & INTERRUPT_MASK_POSTWAIT)) {
    return ZX_ERR_INVALID_ARGS;
  }

  port_dispatcher_ = ktl::move(port_dispatcher);
  port_packet_.key = key;

  if (state_ == InterruptState::TRIGGERED) {
    SendPacketLocked(timestamp_);
    state_ = InterruptState::NEEDACK;
  }
  return ZX_OK;
}

zx_status_t InterruptDispatcher::Unbind(fbl::RefPtr<PortDispatcher> port_dispatcher) {
  // Moving port_dispatcher_ to the local variable ensures it will not be destroyed while
  // holding this spinlock.
  fbl::RefPtr<PortDispatcher> dispatcher;
  {
    Guard<SpinLock, IrqSave> guard{&spinlock_};
    if (port_dispatcher_ != port_dispatcher) {
      // This case also covers the HasVcpu() case.
      return ZX_ERR_NOT_FOUND;
    }
    if (state_ == InterruptState::DESTROYED) {
      return ZX_ERR_CANCELED;
    }
    // Remove the packet for this interrupt from this port on an unbind before actually
    // doing the unbind. This protects against the case where the interrupt dispatcher
    // goes away between an unbind and a port_wait.
    port_dispatcher_->RemoveInterruptPacket(&port_packet_);
    port_packet_.key = 0;
    dispatcher.swap(port_dispatcher_);
  }
  return ZX_OK;
}

zx_status_t InterruptDispatcher::Ack() {
  zx::result<InterruptDispatcher::PostAckState> res = AckInternal();
  return res.is_error() ? res.error_value() : ZX_OK;
}

zx::result<InterruptDispatcher::PostAckState> InterruptDispatcher::AckInternal() {
  PostAckState post_ack_state = FullyAcked;
  bool defer_unmask = false;
  // Use preempt disable to reduce the likelihood of the woken thread running
  // while the spinlock is still held.
  AutoPreemptDisabler preempt_disable;
  {
    Guard<SpinLock, IrqSave> guard{&spinlock_};
    if (port_dispatcher_ == nullptr && !(flags_ & INTERRUPT_ALLOW_ACK_WITHOUT_PORT_FOR_TEST)) {
      return zx::error(ZX_ERR_BAD_STATE);
    }
    if (state_ == InterruptState::DESTROYED) {
      return zx::error(ZX_ERR_CANCELED);
    }
    if (state_ == InterruptState::NEEDACK) {
      if (is_wake_vector()) {
        wake_event_.Acknowledge();
      }
      if (flags_ & INTERRUPT_UNMASK_PREWAIT) {
        UnmaskInterrupt();
      } else if (flags_ & INTERRUPT_UNMASK_PREWAIT_UNLOCKED) {
        defer_unmask = true;
      }
      if (timestamp_) {
        if (!SendPacketLocked(timestamp_)) {
          // We cannot queue another packet here.
          // If we reach here it means that the
          // interrupt packet has not been processed,
          // another interrupt has occurred & then the
          // interrupt was ACK'd
          return zx::error(ZX_ERR_BAD_STATE);
        }
        post_ack_state = PostAckState::Retriggered;
      } else {
        state_ = InterruptState::IDLE;
      }
    }
  }

  if (defer_unmask) {
    UnmaskInterrupt();
  }
  return zx::ok(post_ack_state);
}

void InterruptDispatcher::on_zero_handles() { Destroy(); }
