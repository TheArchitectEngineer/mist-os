// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::memory::MemoryObject;
use crate::mm::{CompareExchangeResult, ProtectionFlags};
use crate::task::{CurrentTask, EventHandler, SignalHandler, SignalHandlerInner, Task, Waiter};
use futures::channel::oneshot;
use starnix_sync::{InterruptibleEvent, Locked, Mutex, Unlocked};
use starnix_types::futex_address::FutexAddress;
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{errno, error, FUTEX_BITSET_MATCH_ANY, FUTEX_TID_MASK, FUTEX_WAITERS};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::{Arc, Weak};

/// A table of futexes.
///
/// Each 32-bit aligned address in an address space can potentially have an associated futex that
/// userspace can wait upon. This table is a sparse representation that has an actual WaitQueue
/// only for those addresses that have ever actually had a futex operation performed on them.
pub struct FutexTable<Key: FutexKey> {
    /// The futexes associated with each address in each VMO.
    ///
    /// This HashMap is populated on-demand when futexes are used.
    state: Mutex<FutexTableState<Key>>,
}

impl<Key: FutexKey> Default for FutexTable<Key> {
    fn default() -> Self {
        Self { state: Mutex::new(FutexTableState::default()) }
    }
}

impl<Key: FutexKey> FutexTable<Key> {
    /// Wait on the futex at the given address given a boot deadline.
    ///
    /// See FUTEX_WAIT when passed a deadline in CLOCK_REALTIME.
    pub fn wait_boot(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        addr: UserAddress,
        value: u32,
        mask: u32,
        deadline: zx::BootInstant,
        timer_slack: zx::BootDuration,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mut state = self.state.lock();
        // As the state is locked, no wake can happen before the waiter is registered.
        // If the addr is remapped, we will read stale data, but we will not miss a futex wake.
        // Acquire ordering to synchronize with userspace modifications to the value on other
        // threads.
        let loaded_value =
            current_task.mm().ok_or_else(|| errno!(EINVAL))?.atomic_load_u32_acquire(addr)?;
        if value != loaded_value {
            return error!(EAGAIN);
        }

        let key = Key::get(current_task, addr)?;
        let waiter = Arc::new(Waiter::new());
        let timer = zx::BootTimer::create();
        let signal_handler = SignalHandler {
            inner: SignalHandlerInner::None,
            event_handler: EventHandler::None,
            err_code: Some(errno!(ETIMEDOUT)),
        };
        waiter
            .wake_on_zircon_signals(&timer, zx::Signals::TIMER_SIGNALED, signal_handler)
            .expect("wait can only fail in OOM conditions");
        timer
            .set(deadline, timer_slack)
            .expect("timer set cannot fail with valid handles and slack");
        state.get_waiters_or_default(key).add(FutexWaiter {
            mask,
            notifiable: FutexNotifiable::new_internal_boot(Arc::downgrade(&waiter)),
        });
        std::mem::drop(state);
        waiter.wait(locked, current_task)
    }

    /// Wait on the futex at the given address.
    ///
    /// See FUTEX_WAIT.
    pub fn wait(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        value: u32,
        mask: u32,
        deadline: zx::MonotonicInstant,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mut state = self.state.lock();
        // As the state is locked, no wake can happen before the waiter is registered.
        // If the addr is remapped, we will read stale data, but we will not miss a futex wake.
        // Acquire ordering to synchronize with userspace modifications to the value on other
        // threads.
        let loaded_value =
            current_task.mm().ok_or_else(|| errno!(EINVAL))?.atomic_load_u32_acquire(addr)?;
        if value != loaded_value {
            return error!(EAGAIN);
        }

        let key = Key::get(current_task, addr)?;
        let event = InterruptibleEvent::new();
        let guard = event.begin_wait();
        state.get_waiters_or_default(key).add(FutexWaiter {
            mask,
            notifiable: FutexNotifiable::new_internal(Arc::downgrade(&event)),
        });
        std::mem::drop(state);

        current_task.block_until(guard, deadline)
    }

    /// Wake the given number of waiters on futex at the given address. Returns the number of
    /// waiters actually woken.
    ///
    /// See FUTEX_WAKE.
    pub fn wake(
        &self,
        task: &Task,
        addr: UserAddress,
        count: usize,
        mask: u32,
    ) -> Result<usize, Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let key = Key::get(task, addr)?;
        Ok(self.state.lock().wake(key, count, mask))
    }

    /// Requeue the waiters to another address.
    ///
    /// See FUTEX_CMP_REQUEUE
    pub fn requeue(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        wake_count: usize,
        requeue_count: usize,
        new_addr: UserAddress,
        expected_value: Option<u32>,
    ) -> Result<usize, Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let new_addr = FutexAddress::try_from(new_addr)?;
        let key = Key::get(current_task, addr)?;
        let new_key = Key::get(current_task, new_addr)?;
        let mut state = self.state.lock();
        if let Some(expected) = expected_value {
            // Use acquire ordering here to synchronize with mutex impls that store w/ release
            // ordering.
            let value =
                current_task.mm().ok_or_else(|| errno!(EINVAL))?.atomic_load_u32_acquire(addr)?;
            if value != expected {
                return error!(EAGAIN);
            }
        }

        let woken;
        let to_requeue;
        match state.waiters.entry(key) {
            Entry::Vacant(_) => return Ok(0),
            Entry::Occupied(mut entry) => {
                // Wake up at most `wake_count` waiters.
                woken = entry.get_mut().notify(FUTEX_BITSET_MATCH_ANY, wake_count);

                // Dequeue up to `requeue_count` waiters to requeue below.
                to_requeue = entry.get_mut().split_for_requeue(requeue_count);

                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }

        let requeued = to_requeue.0.len();
        if !to_requeue.is_empty() {
            state.get_waiters_or_default(new_key).transfer(to_requeue);
        }

        Ok(woken + requeued)
    }

    /// Lock the futex at the given address.
    ///
    /// See FUTEX_LOCK_PI.
    pub fn lock_pi(
        &self,
        current_task: &CurrentTask,
        addr: UserAddress,
        deadline: zx::MonotonicInstant,
    ) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mut state = self.state.lock();
        // As the state is locked, no unlock can happen before the waiter is registered.
        // If the addr is remapped, we will read stale data, but we will not miss a futex unlock.
        let key = Key::get(current_task, addr)?;

        let tid = current_task.get_tid() as u32;
        let mm = current_task.mm().ok_or_else(|| errno!(EINVAL))?;

        // Use a relaxed ordering because the compare/exchange below creates a synchronization
        // point with userspace threads in the success case. No synchronization is required in
        // failure cases.
        let mut current_value = mm.atomic_load_u32_relaxed(addr)?;
        let new_owner_tid = loop {
            let new_owner_tid = current_value & FUTEX_TID_MASK;
            if new_owner_tid == tid {
                // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
                //
                //   EDEADLK
                //          (FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI,
                //          FUTEX_CMP_REQUEUE_PI) The futex word at uaddr is already
                //          locked by the caller.
                return error!(EDEADLOCK);
            }

            if current_value == 0 {
                // Use acq/rel ordering to synchronize with acquire ordering on userspace lock ops
                // and with the release ordering on userspace unlock ops.
                match mm.atomic_compare_exchange_weak_u32_acq_rel(addr, current_value, tid) {
                    CompareExchangeResult::Success => return Ok(()),
                    CompareExchangeResult::Stale { observed } => {
                        current_value = observed;
                        continue;
                    }
                    CompareExchangeResult::Error(e) => return Err(e),
                }
            }

            // Use acq/rel ordering to synchronize with acquire ordering on userspace lock ops and
            // with the release ordering on userspace unlock ops.
            let target_value = current_value | FUTEX_WAITERS;
            match mm.atomic_compare_exchange_u32_acq_rel(addr, current_value, target_value) {
                CompareExchangeResult::Success => (),
                CompareExchangeResult::Stale { observed } => {
                    current_value = observed;
                    continue;
                }
                CompareExchangeResult::Error(e) => return Err(e),
            }
            break new_owner_tid;
        };

        let event = InterruptibleEvent::new();
        let guard = event.begin_wait();
        let notifiable = FutexNotifiable::new_internal(Arc::downgrade(&event));
        state.get_rt_mutex_waiters_or_default(key).push_back(RtMutexWaiter { tid, notifiable });
        std::mem::drop(state);

        // ESRCH  (FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI,
        //        FUTEX_CMP_REQUEUE_PI) The thread ID in the futex word at
        //        uaddr does not exist.
        let new_owner = current_task
            .get_task(new_owner_tid as i32)
            .upgrade()
            .map(|o| o.thread.read().as_ref().map(Arc::clone))
            .flatten()
            .ok_or_else(|| errno!(ESRCH))?;
        current_task.block_with_owner_until(guard, &new_owner, deadline)
    }

    /// Unlock the futex at the given address.
    ///
    /// See FUTEX_UNLOCK_PI.
    pub fn unlock_pi(&self, current_task: &CurrentTask, addr: UserAddress) -> Result<(), Errno> {
        let addr = FutexAddress::try_from(addr)?;
        let mut state = self.state.lock();
        let tid = current_task.get_tid() as u32;
        let mm = current_task.mm().ok_or_else(|| errno!(EINVAL))?;

        let key = Key::get(current_task, addr)?;

        // Use a relaxed ordering because the compare/exchange below creates a synchronization
        // point with userspace threads in the success case. No synchronization is required in
        // failure cases.
        let current_value = mm.atomic_load_u32_relaxed(addr)?;
        if current_value & FUTEX_TID_MASK != tid {
            // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
            //
            //   EPERM  (FUTEX_UNLOCK_PI) The caller does not own the lock
            //          represented by the futex word.
            return error!(EPERM);
        }

        loop {
            let maybe_waiter = state.pop_rt_mutex_waiter(key.clone());
            let target_value = if let Some(waiter) = &maybe_waiter { waiter.tid } else { 0 };

            // Use acq/rel ordering to synchronize with acquire ordering on userspace lock ops and
            // with the release ordering on userspace unlock ops.
            match mm.atomic_compare_exchange_u32_acq_rel(addr, current_value, target_value) {
                CompareExchangeResult::Success => (),
                // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
                //
                //   EINVAL (FUTEX_LOCK_PI, FUTEX_LOCK_PI2, FUTEX_TRYLOCK_PI,
                //       FUTEX_UNLOCK_PI) The kernel detected an inconsistency
                //       between the user-space state at uaddr and the kernel
                //       state.  This indicates either state corruption or that the
                //       kernel found a waiter on uaddr which is waiting via
                //       FUTEX_WAIT or FUTEX_WAIT_BITSET.
                CompareExchangeResult::Stale { .. } => return error!(EINVAL),
                // From <https://man7.org/linux/man-pages/man2/futex.2.html>:
                //
                //   EACCES No read access to the memory of a futex word.
                CompareExchangeResult::Error(_) => return error!(EACCES),
            }

            let Some(mut waiter) = maybe_waiter else {
                // We can stop trying to notify a thread if there are no more waiters.
                break;
            };

            if waiter.notifiable.notify() {
                break;
            }

            // If we couldn't notify the waiter, then we need to pull the next thread off the
            // waiter list.
        }

        Ok(())
    }
}

impl FutexTable<SharedFutexKey> {
    /// Wait on the futex at the given offset in the memory.
    ///
    /// See FUTEX_WAIT.
    pub fn external_wait(
        &self,
        memory: MemoryObject,
        offset: u64,
        value: u32,
        mask: u32,
    ) -> Result<oneshot::Receiver<()>, Errno> {
        let key = SharedFutexKey::new(&memory, offset);
        let mut state = self.state.lock();
        // As the state is locked, no wake can happen before the waiter is registered.
        Self::external_check_futex_value(&memory, offset, value)?;

        let (sender, receiver) = oneshot::channel::<()>();
        state
            .get_waiters_or_default(key)
            .add(FutexWaiter { mask, notifiable: FutexNotifiable::new_external(sender) });
        Ok(receiver)
    }

    /// Wake the given number of waiters on futex at the given offset in the memory. Returns the
    /// number of waiters actually woken.
    ///
    /// See FUTEX_WAKE.
    pub fn external_wake(
        &self,
        memory: MemoryObject,
        offset: u64,
        count: usize,
        mask: u32,
    ) -> Result<usize, Errno> {
        Ok(self.state.lock().wake(SharedFutexKey::new(&memory, offset), count, mask))
    }

    fn external_check_futex_value(
        memory: &MemoryObject,
        offset: u64,
        value: u32,
    ) -> Result<(), Errno> {
        let loaded_value = {
            // TODO: This read should be atomic.
            let mut buf = [0u8; 4];
            memory.read(&mut buf, offset).map_err(|_| errno!(EINVAL))?;
            u32::from_ne_bytes(buf)
        };
        if loaded_value != value {
            return error!(EAGAIN);
        }
        Ok(())
    }
}

pub trait FutexKey: Sized + Ord + Hash + Clone {
    fn get(task: &Task, addr: FutexAddress) -> Result<Self, Errno>;
    fn get_table_from_task(task: &Task) -> Result<&FutexTable<Self>, Errno>;
}

#[derive(Debug, Clone, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct PrivateFutexKey {
    addr: FutexAddress,
}

impl FutexKey for PrivateFutexKey {
    fn get(_task: &Task, addr: FutexAddress) -> Result<Self, Errno> {
        Ok(PrivateFutexKey { addr })
    }

    fn get_table_from_task(task: &Task) -> Result<&FutexTable<Self>, Errno> {
        Ok(&task.mm().ok_or_else(|| errno!(EINVAL))?.futex)
    }
}

#[derive(Debug, Clone, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct SharedFutexKey {
    // No chance of collisions since koids are never reused:
    // https://fuchsia.dev/fuchsia-src/concepts/kernel/concepts#kernel_object_ids
    koid: zx::Koid,
    offset: u64,
}

impl FutexKey for SharedFutexKey {
    fn get(task: &Task, addr: FutexAddress) -> Result<Self, Errno> {
        let (memory, offset) = task
            .mm()
            .ok_or_else(|| errno!(EINVAL))?
            .get_mapping_memory(addr.into(), ProtectionFlags::READ)?;
        Ok(SharedFutexKey::new(&memory, offset))
    }

    fn get_table_from_task(task: &Task) -> Result<&FutexTable<Self>, Errno> {
        Ok(&task.kernel().shared_futexes)
    }
}

impl SharedFutexKey {
    fn new(memory: &MemoryObject, offset: u64) -> Self {
        Self { koid: memory.get_koid(), offset }
    }
}

struct FutexTableState<Key: FutexKey> {
    waiters: HashMap<Key, FutexWaiters>,
    rt_mutex_waiters: HashMap<Key, VecDeque<RtMutexWaiter>>,
}

impl<Key: FutexKey> Default for FutexTableState<Key> {
    fn default() -> Self {
        Self { waiters: Default::default(), rt_mutex_waiters: Default::default() }
    }
}

impl<Key: FutexKey> FutexTableState<Key> {
    /// Returns the FutexWaiters for a given address, creating an empty one if none is registered.
    fn get_waiters_or_default(&mut self, key: Key) -> &mut FutexWaiters {
        self.waiters.entry(key).or_default()
    }

    fn wake(&mut self, key: Key, count: usize, mask: u32) -> usize {
        let entry = self.waiters.entry(key);
        match entry {
            Entry::Vacant(_) => 0,
            Entry::Occupied(mut entry) => {
                let count = entry.get_mut().notify(mask, count);
                if entry.get().is_empty() {
                    entry.remove();
                }
                count
            }
        }
    }

    /// Returns the RT-Mutex waiters queue for a given address, creating an empty queue if none is
    /// registered.
    fn get_rt_mutex_waiters_or_default(&mut self, key: Key) -> &mut VecDeque<RtMutexWaiter> {
        self.rt_mutex_waiters.entry(key).or_default()
    }

    /// Pop the next RT-Mutex for the given address.
    fn pop_rt_mutex_waiter(&mut self, key: Key) -> Option<RtMutexWaiter> {
        let entry = self.rt_mutex_waiters.entry(key);
        match entry {
            Entry::Vacant(_) => None,
            Entry::Occupied(mut entry) => {
                if let Some(mut waiter) = entry.get_mut().pop_front() {
                    if entry.get().is_empty() {
                        entry.remove();
                    } else {
                        waiter.tid |= FUTEX_WAITERS;
                    }
                    Some(waiter)
                } else {
                    None
                }
            }
        }
    }
}

/// Abstraction over a process waiting on a Futex that can be notified.
enum FutexNotifiable {
    /// An internal process waiting on a Futex.
    Internal(Weak<InterruptibleEvent>),
    // An internal process waiting on a Futex with a boot deadline.
    InternalBoot(Weak<Waiter>),
    /// An external process waiting on a Futex.
    // The sender needs to be an option so that one can send the notification while only holding a
    // mut reference on the ExternalWaiter.
    External(Option<oneshot::Sender<()>>),
}

impl FutexNotifiable {
    fn new_internal(event: Weak<InterruptibleEvent>) -> Self {
        Self::Internal(event)
    }

    fn new_internal_boot(waiter: Weak<Waiter>) -> Self {
        Self::InternalBoot(waiter)
    }

    fn new_external(sender: oneshot::Sender<()>) -> Self {
        Self::External(Some(sender))
    }

    /// Tries to notify the process. Returns `true` is the process have been notified. Returns
    /// `false` otherwise. This means the process is stale and will never be available again.
    fn notify(&mut self) -> bool {
        match self {
            Self::Internal(event) => {
                if let Some(event) = event.upgrade() {
                    event.notify();
                    true
                } else {
                    false
                }
            }
            Self::InternalBoot(waiter) => {
                if let Some(waiter) = waiter.upgrade() {
                    waiter.notify();
                    true
                } else {
                    false
                }
            }
            Self::External(ref mut sender) => {
                if let Some(sender) = sender.take() {
                    sender.send(()).is_ok()
                } else {
                    false
                }
            }
        }
    }
}

struct FutexWaiter {
    mask: u32,
    notifiable: FutexNotifiable,
}

#[derive(Default)]
struct FutexWaiters(VecDeque<FutexWaiter>);

impl FutexWaiters {
    fn add(&mut self, waiter: FutexWaiter) {
        self.0.push_back(waiter);
    }

    fn notify(&mut self, mask: u32, count: usize) -> usize {
        let mut woken = 0;
        self.0.retain_mut(|waiter| {
            if woken == count || waiter.mask & mask == 0 {
                return true;
            }
            // The send will fail if the receiver is gone, which means nothing was actualling
            // waiting on the futex.
            if waiter.notifiable.notify() {
                woken += 1;
            }
            false
        });
        woken
    }

    fn transfer(&mut self, mut other: Self) {
        self.0.append(&mut other.0);
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn split_for_requeue(&mut self, max_count: usize) -> Self {
        let pos = if max_count >= self.0.len() { 0 } else { self.0.len() - max_count };
        FutexWaiters(self.0.split_off(pos))
    }
}

struct RtMutexWaiter {
    /// The tid, possibly with the FUTEX_WAITERS bit set.
    tid: u32,

    notifiable: FutexNotifiable,
}
