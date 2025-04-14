// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use std::collections::{HashMap, HashSet};
use std::iter;
use std::sync::{mpsc, Arc, Weak};

use attribution_server::{AttributionServer, AttributionServerHandle};
use fidl::AsHandleRef;
use fidl_fuchsia_memory_attribution as fattribution;
use starnix_logging::log_error;
use starnix_sync::Mutex;
use starnix_uapi::pid_t;
use zx::HandleBased;

use crate::mm::MemoryManager;
use crate::task::{Kernel, ThreadGroup};

/// If the PID table updates multiple times within this interval, we only send an update once, to
/// reduce overhead.
const MINIMUM_RESCAN_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);

#[derive(Debug, Hash, PartialEq, Eq)]
enum MemoryAttributionLifecycleEventType {
    Creation,
    NameChange,
    Destruction,
}

#[derive(Debug)]
pub struct MemoryAttributionLifecycleEvent {
    pid: pid_t,
    event_type: MemoryAttributionLifecycleEventType,
}

impl MemoryAttributionLifecycleEvent {
    pub fn creation(pid: pid_t) -> Self {
        MemoryAttributionLifecycleEvent {
            pid,
            event_type: MemoryAttributionLifecycleEventType::Creation,
        }
    }

    pub fn name_change(pid: pid_t) -> Self {
        MemoryAttributionLifecycleEvent {
            pid,
            event_type: MemoryAttributionLifecycleEventType::NameChange,
        }
    }

    pub fn destruction(pid: pid_t) -> Self {
        MemoryAttributionLifecycleEvent {
            pid,
            event_type: MemoryAttributionLifecycleEventType::Destruction,
        }
    }
}

pub struct MemoryAttributionManager {
    /// Holds state for the hanging-get attribution protocol.
    memory_attribution_server: AttributionServerHandle,
}

struct InitialState {
    /// The initial set of processes running in this kernel.
    processes: HashSet<pid_t>,
}

impl MemoryAttributionManager {
    pub fn new(kernel: Weak<Kernel>) -> Self {
        let (publisher_tx, publisher_rx) = mpsc::sync_channel(1);
        let weak_kernel = kernel;
        let publisher_rx = Arc::new(Mutex::new(Some(publisher_rx)));
        let memory_attribution_server = AttributionServer::new(Box::new(move || {
            // Initial scan of the PID table when a client connects.
            let mut events = vec![];
            let Some(kernel) = weak_kernel.upgrade() else { return vec![] };
            let pids = kernel.pids.read();
            let mut processes: HashSet<pid_t> = HashSet::new();
            for thread_group in pids.get_thread_groups() {
                let name = get_thread_group_identifier(&thread_group);
                events.append(&mut attribution_info_for_thread_group(name, &thread_group));
                processes.insert(thread_group.leader);
            }
            drop(pids);

            // Spawn the pid table monitoring thread once.
            if let Some(publisher_rx) = publisher_rx.lock().take() {
                let (initial_state_tx, initial_state_rx) = mpsc::sync_channel(1);
                initial_state_tx.send(InitialState { processes }).unwrap();
                let weak_kernel = weak_kernel.clone();

                let (pid_sender, pid_receiver) = std::sync::mpsc::channel();

                kernel.kthreads.spawn(move |_, _| {
                    Self::run(weak_kernel, publisher_rx, initial_state_rx, pid_receiver);
                });
                kernel.pids.write().set_thread_group_notifier(pid_sender);
            }
            events
        }));

        let publisher = memory_attribution_server.new_publisher();
        _ = publisher_tx.send(publisher);

        Self { memory_attribution_server }
    }

    pub fn new_observer(
        &self,
        control_handle: fattribution::ProviderControlHandle,
    ) -> attribution_server::Observer {
        self.memory_attribution_server.new_observer(control_handle)
    }

    /// Monitor the kernel for incremental memory attribution updates and
    /// publish them via the memory update publisher.
    ///
    /// ## Arguments
    ///
    /// - kernel: Weak reference to the kernel state.
    /// - publisher: Receiver for a handle used to publish memory attribution updates.
    /// - initial_state: Receiver for the initial state of attribution.
    /// - waiter: Used to wait for thread group changes.
    ///
    fn run(
        kernel: Weak<Kernel>,
        publisher: mpsc::Receiver<attribution_server::Publisher>,
        initial_state: mpsc::Receiver<InitialState>,
        pid_receiver: mpsc::Receiver<MemoryAttributionLifecycleEvent>,
    ) {
        let publisher = publisher.recv().unwrap();
        let initial_state = initial_state.recv().unwrap();
        let InitialState { processes } = initial_state;

        let Some(kernel) = kernel.upgrade() else {
            return;
        };

        let (mut processes, updates) = scan_processes(&kernel, processes);
        // If there are updates to send, send them now.
        if !updates.is_empty() {
            _ = publisher.on_update(updates);
        }

        loop {
            // There may be multiple pending notifications in the receiving channel. We would like
            // to process them all at once. For that, we first wait up to the timeout for any event,
            // then we try to pull as many events as available from the channel, until it is empty.
            // We keep track whether we hit a timeout to do a full scan; if the full scan finds some
            // updates to send, it means we have uninstrumented sources of changes that need to be
            // fixed.
            let events = match pid_receiver.recv() {
                Ok(v) => itertools::chain(std::iter::once(v), pid_receiver.try_iter()),
                Err(_) => {
                    return;
                }
            }
            .fold(
                HashMap::new(),
                |mut acc: HashMap<pid_t, MemoryAttributionLifecycleEventType>, i| {
                    // We don't need to send all events: only one per pid is necessary at most. For
                    // instance:
                    // - We don't need to send any update if the same thread group is created and
                    // destroyed;
                    // - We don't need to send a name change event if the thread group is also
                    // created (the creation event will bear the right name), or destroyed.
                    let entry = acc.entry(i.pid);
                    match entry {
                        std::collections::hash_map::Entry::Occupied(mut occupied_entry) => {
                            match occupied_entry.get() {
                                MemoryAttributionLifecycleEventType::Creation => match i.event_type
                                {
                                    MemoryAttributionLifecycleEventType::Creation => occupied_entry
                                        .insert(MemoryAttributionLifecycleEventType::Creation),
                                    MemoryAttributionLifecycleEventType::NameChange => {
                                        occupied_entry
                                            .insert(MemoryAttributionLifecycleEventType::Creation)
                                    }
                                    MemoryAttributionLifecycleEventType::Destruction => {
                                        occupied_entry.remove()
                                    }
                                },
                                MemoryAttributionLifecycleEventType::NameChange => match i
                                    .event_type
                                {
                                    MemoryAttributionLifecycleEventType::Creation => occupied_entry
                                        .insert(MemoryAttributionLifecycleEventType::Creation),
                                    MemoryAttributionLifecycleEventType::NameChange => {
                                        occupied_entry
                                            .insert(MemoryAttributionLifecycleEventType::NameChange)
                                    }
                                    MemoryAttributionLifecycleEventType::Destruction => {
                                        occupied_entry.insert(
                                            MemoryAttributionLifecycleEventType::Destruction,
                                        )
                                    }
                                },
                                MemoryAttributionLifecycleEventType::Destruction => match i
                                    .event_type
                                {
                                    MemoryAttributionLifecycleEventType::Creation => occupied_entry
                                        .insert(MemoryAttributionLifecycleEventType::Creation),
                                    MemoryAttributionLifecycleEventType::NameChange => {
                                        occupied_entry.insert(
                                            MemoryAttributionLifecycleEventType::Destruction,
                                        )
                                    }
                                    MemoryAttributionLifecycleEventType::Destruction => {
                                        occupied_entry.insert(
                                            MemoryAttributionLifecycleEventType::Destruction,
                                        )
                                    }
                                },
                            };
                        }
                        std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                            vacant_entry.insert(i.event_type);
                        }
                    }
                    acc
                },
            );

            // Creation events create two updates; with this, we are sure to have enough capacity.
            let mut updates = Vec::with_capacity(2 * events.len());
            for (pid, event_type) in events {
                match event_type {
                    MemoryAttributionLifecycleEventType::Creation => {
                        if !processes.insert(pid) {
                            log_error!(
                                "{} is already known, memory attribution is likely incorrect",
                                pid
                            );
                        }
                        // It is faster to take the lock multiple times for short durations than to
                        // take it for the whole for loop.
                        let pid_table = kernel.pids.read();
                        let thread_group = match pid_table.get_thread_group(pid) {
                            Some(tg) => tg,
                            None => {
                                // The thread group is missing. This can happen if it has already
                                // exited.
                                continue;
                            }
                        };
                        let name = get_thread_group_identifier(&thread_group);
                        let mut update = attribution_info_for_thread_group(name, &thread_group);
                        updates.append(&mut update);
                    }
                    MemoryAttributionLifecycleEventType::NameChange => {
                        if !processes.contains(&pid) {
                            log_error!(
                                "{} is unknown, memory attribution is likely incorrect",
                                pid
                            );
                        }
                        let pid_table = kernel.pids.read();
                        let thread_group = match pid_table.get_thread_group(pid) {
                            Some(tg) => tg,
                            None => continue,
                        };
                        let name = get_thread_group_identifier(&thread_group);
                        updates.push(new_principal(thread_group.leader, name));
                    }
                    MemoryAttributionLifecycleEventType::Destruction => {
                        if !processes.remove(&pid) {
                            log_error!(
                                "{} is unknown, memory attribution is likely incorrect",
                                pid
                            );
                        }
                        updates.push(fattribution::AttributionUpdate::Remove(pid as u64));
                    }
                }
            }

            // If there are updates to send, send them now.
            if !updates.is_empty() {
                _ = publisher.on_update(updates);
            }
            zx::MonotonicInstant::after(MINIMUM_RESCAN_INTERVAL).sleep();
        }
    }
}

/// Do a full scan of the current Starnix processes. This is useful to establish an initial state.
fn scan_processes(
    kernel: &Kernel,
    mut processes: HashSet<pid_t>,
) -> (HashSet<pid_t>, Vec<fattribution::AttributionUpdate>) {
    let mut updates = vec![];
    let pids = kernel.pids.read();
    let mut new_processes = HashSet::new();
    for thread_group in pids.get_thread_groups() {
        let pid = thread_group.leader;
        new_processes.insert(pid);
        // TODO(https://fxbug.dev/379733655): Remove this
        #[allow(clippy::set_contains_or_insert)]
        if !processes.contains(&pid) {
            let name = get_thread_group_identifier(&thread_group);
            let mut update = attribution_info_for_thread_group(name, &thread_group);
            processes.insert(pid);
            updates.append(&mut update);
        }
    }

    for pid in processes.difference(&new_processes) {
        updates.push(fattribution::AttributionUpdate::Remove(*pid as u64));
    }
    (new_processes, updates)
}

fn get_thread_group_identifier(thread_group: &ThreadGroup) -> String {
    let name = match thread_group.process.is_invalid_handle() {
        // The system task has an invalid Zircon process handle.
        true => zx::Name::new_lossy("[system task]"),
        false => thread_group.process.get_name().unwrap_or_default(),
    };
    let id = thread_group.leader;
    let name = format!("{id}: {name}");
    name
}

fn attribution_info_for_thread_group(
    name: String,
    thread_group: &ThreadGroup,
) -> Vec<fattribution::AttributionUpdate> {
    let new = new_principal(thread_group.leader, name);
    let updated = updated_principal(thread_group);
    iter::once(new).chain(updated.into_iter()).collect()
}

/// Builds a `NewPrincipal` event.
fn new_principal(pid: i32, name: String) -> fattribution::AttributionUpdate {
    let new = fattribution::AttributionUpdate::Add(fattribution::NewPrincipal {
        identifier: Some(pid as u64),
        description: Some(fattribution::Description::Part(name)),
        principal_type: Some(fattribution::PrincipalType::Runnable),
        detailed_attribution: None,
        ..Default::default()
    });
    new
}

/// Builds an `UpdatedPrincipal` event. If the task has an invalid root VMAR, returns `None`.
fn updated_principal(thread_group: &ThreadGroup) -> Option<fattribution::AttributionUpdate> {
    let Some(process_koid) = thread_group.process.get_koid().ok() else {
        return None;
    };
    let Some(mm) = get_mm(thread_group) else {
        log_error!(
            "No memory manager for ThreadGroup {}, this should not happen.",
            thread_group.leader
        );
        return None;
    };
    let Some(vmar_info) = mm.get_restricted_vmar_info() else {
        return None;
    };
    let update = fattribution::AttributionUpdate::Update(fattribution::UpdatedPrincipal {
        identifier: Some(thread_group.leader as u64),
        resources: Some(fattribution::Resources::Data(fattribution::Data {
            resources: vec![fattribution::Resource::ProcessMapped(fattribution::ProcessMapped {
                process: process_koid.raw_koid(),
                base: vmar_info.base as u64,
                len: vmar_info.len as u64,
            })],
        })),
        ..Default::default()
    });
    Some(update)
}

/// Get the memory manager shared by tasks in the thread group.
///
/// If all tasks in the thread group has been killed, returns `None`.
fn get_mm(thread_group: &ThreadGroup) -> Option<Arc<MemoryManager>> {
    thread_group.read().tasks().find_map(|task| task.mm().cloned())
}
