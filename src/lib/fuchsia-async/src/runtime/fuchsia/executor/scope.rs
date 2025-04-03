// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::super::task::JoinHandle;
use super::atomic_future::{AtomicFutureHandle, CancelAndDetachResult};
use super::common::{Executor, TaskHandle};
use crate::condition::{Condition, ConditionGuard, WakerEntry};
use crate::EHandle;
use fuchsia_sync::Mutex;
use futures::Stream;
use pin_project_lite::pin_project;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use state::{JoinResult, ScopeState, ScopeWaker, Status};
use std::any::Any;
use std::borrow::Borrow;
use std::collections::hash_map::Entry;
use std::collections::hash_set;
use std::future::{Future, IntoFuture};
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{ready, Context, Poll, Waker};
use std::{fmt, hash};

//
// # Public API
//

/// A scope for managing async tasks. This scope is cancelled when dropped.
///
/// Scopes are how fuchsia-async implements [structured concurrency][sc]. Every
/// task is spawned on a scope, and runs until either the task completes or the
/// scope is cancelled. In addition to owning tasks, scopes may own child
/// scopes, forming a nested structure.
///
/// Scopes are usually joined or cancelled when the owning code is done with
/// them. This makes it easier to reason about when a background task might
/// still be running. Note that in multithreaded contexts it is safer to cancel
/// and await a scope explicitly than to drop it, because the destructor is not
/// synchronized with other threads that might be running a task.
///
/// [`Task::spawn`][crate::Task::spawn] and related APIs spawn on the root scope
/// of the executor. New code is encouraged to spawn directly on scopes instead,
/// passing their handles as a way of documenting when a function might spawn
/// tasks that run in the background and reasoning about their side effects.
///
/// ## Scope lifecycle
///
/// When a scope is created it is open, meaning it accepts new tasks. Scopes are
/// closed when one of the following happens:
///
/// 1. When [`close()`][Scope::close] is called.
/// 2. When the scope is cancelled or dropped, the scope is closed immediately.
/// 3. When the scope is joined and all tasks complete, the scope is closed
///    before the join future resolves.
///
/// When a scope is closed it no longer accepts tasks. Tasks spawned on the
/// scope are dropped immediately, and their [`Task`][crate::Task] or
/// [`JoinHandle`][crate::JoinHandle] futures never resolve. This applies
/// transitively to all child scopes. Closed scopes cannot currently be
/// reopened.
///
/// Scopes can also be detached, in which case they are never closed, and run
/// until the completion of all tasks.
///
/// [sc]: https://en.wikipedia.org/wiki/Structured_concurrency
#[must_use = "Scopes should be explicitly awaited or cancelled"]
#[derive(Debug)]
pub struct Scope {
    // LINT.IfChange
    inner: ScopeHandle,
    // LINT.ThenChange(//src/developer/debug/zxdb/console/commands/verb_async_backtrace.cc)
}

impl Scope {
    /// Create a new scope.
    ///
    /// The returned scope is a child of the current scope.
    ///
    /// # Panics
    ///
    /// May panic if not called in the context of an executor (e.g. within a
    /// call to [`run`][crate::SendExecutor::run]).
    pub fn new() -> Scope {
        ScopeHandle::with_current(|handle| handle.new_child())
    }

    /// Create a new scope with a name.
    ///
    /// The returned scope is a child of the current scope.
    ///
    /// # Panics
    ///
    /// May panic if not called in the context of an executor (e.g. within a
    /// call to [`run`][crate::SendExecutor::run]).
    pub fn new_with_name(name: &str) -> Scope {
        ScopeHandle::with_current(|handle| handle.new_child_with_name(name))
    }

    /// Get the scope of the current task, or the global scope if there is no task
    /// being polled.
    ///
    /// # Panics
    ///
    /// May panic if not called in the context of an executor (e.g. within a
    /// call to [`run`][crate::SendExecutor::run]).
    pub fn current() -> ScopeHandle {
        ScopeHandle::with_current(|handle| handle.clone())
    }

    /// Get the global scope of the executor.
    ///
    /// This can be used to spawn tasks that live as long as the executor.
    /// Usually, this means until the end of the program or test. This should
    /// only be done for tasks where this is expected. If in doubt, spawn on a
    /// shorter lived scope instead.
    ///
    /// In code that uses scopes, you are strongly encouraged to use this API
    /// instead of the spawn APIs on [`Task`][crate::Task].
    ///
    /// All scopes are descendants of the global scope.
    ///
    /// # Panics
    ///
    /// May panic if not called in the context of an executor (e.g. within a
    /// call to [`run`][crate::SendExecutor::run]).
    pub fn global() -> ScopeHandle {
        EHandle::local().global_scope().clone()
    }

    /// Create a child scope.
    pub fn new_child(&self) -> Scope {
        self.inner.new_child()
    }

    /// Create a child scope with a name.
    pub fn new_child_with_name(&self, name: &str) -> Scope {
        self.inner.new_child_with_name(name)
    }

    /// Returns the name of the scope.
    pub fn name(&self) -> &str {
        &self.inner.inner.name
    }

    /// Create a [`ScopeHandle`] that may be used to spawn tasks on this scope.
    ///
    /// This is a shorthand for `scope.as_handle().clone()`.
    ///
    /// Scope holds a `ScopeHandle` and implements Deref to make its methods
    /// available. Note that you should _not_ call `scope.clone()`, even though
    /// the compiler allows it due to the Deref impl. Call this method instead.
    pub fn to_handle(&self) -> ScopeHandle {
        self.inner.clone()
    }

    /// Get a reference to a [`ScopeHandle`] that may be used to spawn tasks on
    /// this scope.
    ///
    /// Scope holds a `ScopeHandle` and implements Deref to make its methods
    /// available. If you have a `Scope` but need a `&ScopeHandle`, prefer
    /// calling this method over the less readable `&*scope`.
    pub fn as_handle(&self) -> &ScopeHandle {
        &self.inner
    }

    /// Wait for all tasks in the scope and its children to complete.
    ///
    /// New tasks will be accepted on the scope until every task completes and
    /// this future resolves.
    ///
    /// Note that you can await a scope directly because it implements
    /// `IntoFuture`. `scope.join().await` is a more explicit form of
    /// `scope.await`.
    pub fn join(self) -> Join {
        Join::new(self)
    }

    /// Stop accepting new tasks on the scope. Returns a future that waits for
    /// every task on the scope to complete.
    pub fn close(self) -> Join {
        self.inner.close();
        Join::new(self)
    }

    /// Cancel all tasks in the scope and its children recursively.
    ///
    /// Once the returned future resolves, no task on the scope will be polled
    /// again.
    ///
    /// When a scope is cancelled it immediately stops accepting tasks. Handles
    /// of tasks spawned on the scope will pend forever.
    ///
    /// Dropping the `Scope` object is equivalent to calling this method and
    /// discarding the returned future. Awaiting the future is preferred because
    /// it eliminates the possibility of a task poll completing on another
    /// thread after the scope object has been dropped, which can sometimes
    /// result in surprising behavior.
    pub fn cancel(self) -> impl Future<Output = ()> {
        self.inner.cancel_all_tasks();
        Join::new(self)
    }

    /// Detach the scope, allowing its tasks to continue running in the
    /// background.
    ///
    /// Tasks of a detached scope are still subject to join and cancel
    /// operations on parent scopes.
    pub fn detach(self) {
        // Use ManuallyDrop to destructure self, because Rust doesn't allow this
        // for types which implement Drop.
        let this = ManuallyDrop::new(self);
        // SAFETY: this.inner is obviously valid, and we don't access `this`
        // after moving.
        mem::drop(unsafe { std::ptr::read(&this.inner) });
    }
}

/// Cancel the scope and all of its tasks. Prefer using the [`Scope::cancel`]
/// or [`Scope::join`] methods.
impl Drop for Scope {
    fn drop(&mut self) {
        // Cancel all tasks in the scope. Each task has a strong reference to the ScopeState,
        // which will be dropped after all the tasks in the scope are dropped.

        // TODO(https://fxbug.dev/340638625): Ideally we would drop all tasks
        // here, but we cannot do that without either:
        // - Sync drop support in AtomicFuture, or
        // - The ability to reparent tasks, which requires atomic_arc or
        //   acquiring a mutex during polling.
        self.inner.cancel_all_tasks();
    }
}

impl IntoFuture for Scope {
    type Output = ();
    type IntoFuture = Join;
    fn into_future(self) -> Self::IntoFuture {
        self.join()
    }
}

impl Deref for Scope {
    type Target = ScopeHandle;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Borrow<ScopeHandle> for Scope {
    fn borrow(&self) -> &ScopeHandle {
        &*self
    }
}

pin_project! {
    /// Join operation for a [`Scope`].
    ///
    /// This is a future that resolves when all tasks on the scope are complete
    /// or have been cancelled. New tasks will be accepted on the scope until
    /// every task completes and this future resolves.
    ///
    /// When this object is dropped, the scope and all tasks in it are
    /// cancelled.
    //
    // Note: The drop property is only true when S = Scope; it does not apply to
    // other (non-public) uses of this struct where S = ScopeHandle.
    pub struct Join<S = Scope> {
        scope: S,
        #[pin]
        waker_entry: WakerEntry<ScopeState>,
    }
}

impl<S> Join<S> {
    fn new(scope: S) -> Self {
        Self { scope, waker_entry: WakerEntry::new() }
    }
}

impl Join {
    /// Cancel the scope. The future will resolve when all tasks have finished
    /// polling.
    ///
    /// See [`Scope::cancel`] for more details.
    pub fn cancel(self: Pin<&mut Self>) -> impl Future<Output = ()> + '_ {
        self.scope.inner.cancel_all_tasks();
        self
    }
}

impl<S> Future for Join<S>
where
    S: Borrow<ScopeHandle>,
{
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let mut state = Borrow::borrow(&*this.scope).lock();
        if state.has_tasks() {
            state.add_waker(this.waker_entry, cx.waker().clone());
            Poll::Pending
        } else {
            state.mark_finished();
            Poll::Ready(())
        }
    }
}

/// Trait for things that can be spawned on to a scope.  There is a blanket implementation
/// below for futures.
pub trait Spawnable {
    /// The type of value produced on completion.
    type Output;

    /// Converts to a task that can be spawned directly.
    fn into_task(self, scope: ScopeHandle) -> TaskHandle;
}

impl<F: Future + Send + 'static> Spawnable for F
where
    F::Output: Send + 'static,
{
    type Output = F::Output;

    fn into_task(self, scope: ScopeHandle) -> TaskHandle {
        scope.new_task(None, self)
    }
}

/// A handle to a scope, which may be used to spawn tasks.
///
/// ## Ownership and cycles
///
/// Tasks running on a `Scope` may hold a `ScopeHandle` to that scope. This does
/// not create an ownership cycle because the task will drop the `ScopeHandle`
/// once it completes or is cancelled.
///
/// Naturally, scopes containing tasks that never complete and that are never
/// cancelled will never be freed. Holding a `ScopeHandle` does not contribute to
/// this problem.
#[derive(Clone)]
pub struct ScopeHandle {
    // LINT.IfChange
    inner: Arc<ScopeInner>,
    // LINT.ThenChange(//src/developer/debug/zxdb/console/commands/verb_async_backtrace.cc)
}

impl ScopeHandle {
    /// Create a child scope.
    pub fn new_child(&self) -> Scope {
        let mut state = self.lock();
        let child = ScopeHandle {
            inner: Arc::new(ScopeInner {
                executor: self.inner.executor.clone(),
                state: Condition::new(ScopeState::new(
                    Some(self.clone()),
                    state.status(),
                    JoinResults::default().into(),
                )),
                name: String::new(),
            }),
        };
        let weak = child.downgrade();
        state.insert_child(weak);
        Scope { inner: child }
    }

    /// Create a child scope.
    pub fn new_child_with_name(&self, name: &str) -> Scope {
        let mut state = self.lock();
        let child = ScopeHandle {
            inner: Arc::new(ScopeInner {
                executor: self.inner.executor.clone(),
                state: Condition::new(ScopeState::new(
                    Some(self.clone()),
                    state.status(),
                    JoinResults::default().into(),
                )),
                name: name.to_string(),
            }),
        };
        let weak = child.downgrade();
        state.insert_child(weak);
        Scope { inner: child }
    }

    /// Spawn a new task on the scope.
    // This does not have the must_use attribute because it's common to detach and the lifetime of
    // the task is bound to the scope: when the scope is dropped, the task will be cancelled.
    pub fn spawn(&self, future: impl Spawnable<Output = ()>) -> JoinHandle<()> {
        let task = future.into_task(self.clone());
        let task_id = task.id();
        self.insert_task(task, false);
        JoinHandle::new(self.clone(), task_id)
    }

    /// Spawn a new task on the scope of a thread local executor.
    ///
    /// NOTE: This is not supported with a [`SendExecutor`][crate::SendExecutor]
    /// and will cause a runtime panic. Use [`ScopeHandle::spawn`] instead.
    pub fn spawn_local(&self, future: impl Future<Output = ()> + 'static) -> JoinHandle<()> {
        let task = self.new_local_task(None, future);
        let id = task.id();
        self.insert_task(task, false);
        JoinHandle::new(self.clone(), id)
    }

    /// Like `spawn`, but for tasks that return a result.
    ///
    /// NOTE: Unlike `spawn`, when tasks are dropped, the future will be
    /// *cancelled*.
    pub fn compute<T: Send + 'static>(
        &self,
        future: impl Spawnable<Output = T> + Send + 'static,
    ) -> crate::Task<T> {
        let task = future.into_task(self.clone());
        let id = task.id();
        self.insert_task(task, false);
        JoinHandle::new(self.clone(), id).into()
    }

    /// Like `spawn`, but for tasks that return a result.
    ///
    /// NOTE: Unlike `spawn`, when tasks are dropped, the future will be
    /// *cancelled*.
    ///
    /// NOTE: This is not supported with a [`SendExecutor`][crate::SendExecutor]
    /// and will cause a runtime panic. Use [`ScopeHandle::spawn`] instead.
    pub fn compute_local<T: 'static>(
        &self,
        future: impl Future<Output = T> + 'static,
    ) -> crate::Task<T> {
        let task = self.new_local_task(None, future);
        let id = task.id();
        self.insert_task(task, false);
        JoinHandle::new(self.clone(), id).into()
    }

    pub(super) fn root(executor: Arc<Executor>) -> ScopeHandle {
        ScopeHandle {
            inner: Arc::new(ScopeInner {
                executor,
                state: Condition::new(ScopeState::new(
                    None,
                    Status::default(),
                    JoinResults::default().into(),
                )),
                name: "root".to_string(),
            }),
        }
    }

    /// Stop the scope from accepting new tasks.
    ///
    /// Note that unlike [`Scope::close`], this does not return a future that
    /// waits for all tasks to complete. This could lead to resource leaks
    /// because it is not uncommon to access a TaskGroup from a task running on
    /// the scope itself. If such a task were to await a future returned by this
    /// method it would suspend forever waiting for itself to complete.
    pub fn close(&self) {
        self.lock().close();
    }

    /// Cancel all the scope's tasks.
    ///
    /// Note that if this is called from within a task running on the scope, the
    /// task will not resume from the next await point.
    pub fn cancel(self) -> impl Future<Output = ()> {
        self.cancel_all_tasks();
        Join::new(self)
    }

    // Joining the scope could be allowed from a ScopeHandle, but the use case
    // seems less common and more bug prone than cancelling. We don't allow this
    // for the same reason we don't return a future from close().

    /// Wait for there to be no tasks. This is racy: as soon as this returns it is possible for
    /// another task to have been spawned on this scope.
    pub async fn on_no_tasks(&self) {
        self.inner
            .state
            .when(|state| if state.has_tasks() { Poll::Pending } else { Poll::Ready(()) })
            .await;
    }

    /// Wake all the scope's tasks so their futures will be polled again.
    pub fn wake_all(&self) {
        self.lock().wake_all();
    }

    /// Creates a new task associated with this scope.  This does not spawn it on the executor.
    /// That must be done separately.
    pub(crate) fn new_task<'a, Fut: Future + Send + 'a>(
        &self,
        id: Option<usize>,
        fut: Fut,
    ) -> AtomicFutureHandle<'a>
    where
        Fut::Output: Send,
    {
        AtomicFutureHandle::new(
            Some(self.clone()),
            id.unwrap_or_else(|| self.executor().next_task_id()),
            fut,
        )
    }

    /// Creates a new task associated with this scope.  This does not spawn it on the executor.
    /// That must be done separately.
    pub(crate) fn new_local_task<'a>(
        &self,
        id: Option<usize>,
        fut: impl Future + 'a,
    ) -> AtomicFutureHandle<'a> {
        // Check that the executor is local.
        if !self.executor().is_local() {
            panic!(
                "Error: called `new_local_task` on multithreaded executor. \
                 Use `spawn` or a `LocalExecutor` instead."
            );
        }

        // SAFETY: We've confirmed that the futures here will never be used across multiple threads,
        // so the Send requirements that `new_local` requires should be met.
        unsafe {
            AtomicFutureHandle::new_local(
                Some(self.clone()),
                id.unwrap_or_else(|| self.executor().next_task_id()),
                fut,
            )
        }
    }
}

impl fmt::Debug for ScopeHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope").field("name", &self.inner.name).finish()
    }
}

/// Similar to a scope but all futures spawned on the scope *must* finish with the same result type.
/// That allows the scope to return a stream of results. Attempting to spawn tasks using
/// `ScopeHandle::spawn` (or similar) will result in tasks that are immediately dropped (just as if
/// the scope was closed).  Like a regular scope, the scope can be closed, at which point the stream
/// will terminate once all the tasks have finished.  This is designed to be a fairly close drop-in
/// replacement to `FuturesUnordered`, the principle difference being that the tasks run in parallel
/// rather than just concurrently.  Another difference is that the futures don't need to be the same
/// type; only the outputs do.  In all other respects, the scope operates like a regular scope i.e.
/// it can have children, you can join them, cancel them, etc.
pub struct ScopeStream<R> {
    inner: ScopeHandle,
    stream: Arc<Mutex<ResultsStreamInner<R>>>,
}

impl<R: Send + 'static> ScopeStream<R> {
    /// Creates a new scope stream.
    ///
    /// The returned scope stream is a child of the current scope.
    ///
    /// # Panics
    ///
    /// May panic if not called in the context of an executor (e.g. within a
    /// call to [`run`][crate::SendExecutor::run]).
    pub fn new() -> (Self, ScopeStreamHandle<R>) {
        Self::new_with_name(String::new())
    }

    /// Creates a new scope stream with a name.
    ///
    /// The returned scope stream is a child of the current scope.
    ///
    /// # Panics
    ///
    /// May panic if not called in the context of an executor (e.g. within a
    /// call to [`run`][crate::SendExecutor::run]).
    pub fn new_with_name(name: String) -> (Self, ScopeStreamHandle<R>) {
        let this = ScopeHandle::with_current(|handle| {
            let mut state = handle.lock();
            let stream = Arc::default();
            let child = ScopeHandle {
                inner: Arc::new(ScopeInner {
                    executor: handle.executor().clone(),
                    state: Condition::new(ScopeState::new(
                        Some(handle.clone()),
                        state.status(),
                        Box::new(ResultsStream { inner: Arc::clone(&stream) }),
                    )),
                    name,
                }),
            };
            let weak = child.downgrade();
            state.insert_child(weak);
            ScopeStream { inner: child, stream }
        });
        let handle = ScopeStreamHandle(this.inner.clone(), PhantomData);
        (this, handle)
    }
}

impl<R> Drop for ScopeStream<R> {
    fn drop(&mut self) {
        // Cancel all tasks in the scope. Each task has a strong reference to the ScopeState,
        // which will be dropped after all the tasks in the scope are dropped.

        // TODO(https://fxbug.dev/340638625): Ideally we would drop all tasks
        // here, but we cannot do that without either:
        // - Sync drop support in AtomicFuture, or
        // - The ability to reparent tasks, which requires atomic_arc or
        //   acquiring a mutex during polling.
        self.inner.cancel_all_tasks();
    }
}

impl<R: Send + 'static> Stream for ScopeStream<R> {
    type Item = R;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut stream_inner = self.stream.lock();
        match stream_inner.results.pop() {
            Some(result) => Poll::Ready(Some(result)),
            None => {
                // Lock ordering: when results are posted, the state lock is taken first, so we must
                // do the same.
                drop(stream_inner);
                let state = self.inner.lock();
                let mut stream_inner = self.stream.lock();
                match stream_inner.results.pop() {
                    Some(result) => Poll::Ready(Some(result)),
                    None => {
                        if state.has_tasks() {
                            stream_inner.waker = Some(cx.waker().clone());
                            Poll::Pending
                        } else {
                            Poll::Ready(None)
                        }
                    }
                }
            }
        }
    }
}

impl<R> Deref for ScopeStream<R> {
    type Target = ScopeHandle;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<R> Borrow<ScopeHandle> for ScopeStream<R> {
    fn borrow(&self) -> &ScopeHandle {
        &*self
    }
}

impl<F: Spawnable<Output = R>, R: Send + 'static> FromIterator<F> for ScopeStream<R> {
    fn from_iter<T: IntoIterator<Item = F>>(iter: T) -> Self {
        let (stream, handle) = ScopeStream::new();
        for fut in iter {
            handle.push(fut);
        }
        stream.close();
        stream
    }
}

#[derive(Clone)]
pub struct ScopeStreamHandle<R>(ScopeHandle, PhantomData<R>);

impl<R: Send> ScopeStreamHandle<R> {
    pub fn push(&self, future: impl Spawnable<Output = R>) {
        self.0.insert_task(future.into_task(self.0.clone()), true);
    }
}

//
// # Internal API
//

/// A weak reference to a scope.
#[derive(Clone)]
struct WeakScopeHandle {
    inner: Weak<ScopeInner>,
}

impl WeakScopeHandle {
    /// Upgrades to a [`ScopeHandle`] if the scope still exists.
    pub fn upgrade(&self) -> Option<ScopeHandle> {
        self.inner.upgrade().map(|inner| ScopeHandle { inner })
    }
}

impl hash::Hash for WeakScopeHandle {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        Weak::as_ptr(&self.inner).hash(state);
    }
}

impl PartialEq for WeakScopeHandle {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for WeakScopeHandle {
    // Weak::ptr_eq should return consistent results, even when the inner value
    // has been dropped.
}

// This module exists as a privacy boundary so that we can make sure any
// operation that might cause the scope to finish also wakes its waker.
mod state {
    use super::*;

    pub struct ScopeState {
        pub parent: Option<ScopeHandle>,
        // LINT.IfChange
        children: HashSet<WeakScopeHandle>,
        all_tasks: HashSet<TaskHandle>,
        // LINT.ThenChange(//src/developer/debug/zxdb/console/commands/verb_async_backtrace.cc)
        /// The number of children that transitively contain tasks, plus one for
        /// this scope if it directly contains tasks.
        subscopes_with_tasks: u32,
        status: Status,
        /// Wakers/results for joining each task.
        pub results: Box<dyn Results>,
    }

    pub enum JoinResult {
        Waker(Waker),
        Result(TaskHandle),
    }

    #[repr(u8)] // So zxdb can read the status.
    #[derive(Default, Debug, Clone, Copy)]
    pub enum Status {
        #[default]
        /// The scope is accepting new tasks.
        Open,
        /// The scope is no longer accepting new tasks.
        Closed,
        /// The scope is not accepting new tasks and all tasks have completed.
        ///
        /// This is purely an optimization; it is not guaranteed to be set.
        Finished,
    }

    impl Status {
        pub fn can_spawn(&self) -> bool {
            match self {
                Status::Open => true,
                Status::Closed | Status::Finished => false,
            }
        }

        pub fn might_have_running_tasks(&self) -> bool {
            match self {
                Status::Open | Status::Closed => true,
                Status::Finished => false,
            }
        }
    }

    impl ScopeState {
        pub fn new(
            parent: Option<ScopeHandle>,
            status: Status,
            results: Box<impl Results>,
        ) -> Self {
            Self {
                parent,
                children: Default::default(),
                all_tasks: Default::default(),
                subscopes_with_tasks: 0,
                status,
                results,
            }
        }
    }

    impl ScopeState {
        pub fn all_tasks(&self) -> &HashSet<TaskHandle> {
            &self.all_tasks
        }

        /// Attempts to add a task to the scope. Returns the task if the scope cannot accept a task
        /// (since it isn't safe to drop the task whilst the lock is held).
        pub fn insert_task(&mut self, task: TaskHandle, for_stream: bool) -> Option<TaskHandle> {
            if !self.status.can_spawn() || (!for_stream && !self.results.can_spawn()) {
                return Some(task);
            }
            if self.all_tasks.is_empty() && !self.register_first_task() {
                return Some(task);
            }
            task.wake();
            assert!(self.all_tasks.insert(task));
            None
        }

        pub fn children(&self) -> &HashSet<WeakScopeHandle> {
            &self.children
        }

        pub fn insert_child(&mut self, child: WeakScopeHandle) {
            self.children.insert(child);
        }

        pub fn remove_child(&mut self, child: &PtrKey) {
            let found = self.children.remove(child);
            // This should always succeed unless the scope is being dropped
            // (in which case children will be empty).
            assert!(found || self.children.is_empty());
        }

        pub fn status(&self) -> Status {
            self.status
        }

        pub fn close(&mut self) {
            self.status = Status::Closed;
        }

        pub fn mark_finished(&mut self) {
            self.status = Status::Finished;
        }

        pub fn has_tasks(&self) -> bool {
            self.subscopes_with_tasks > 0
        }

        pub fn wake_all(&self) {
            for task in &self.all_tasks {
                task.wake();
            }
        }

        /// Registers our first task with the parent scope.
        ///
        /// Returns false if the scope is not allowed to accept a task.
        #[must_use]
        fn register_first_task(&mut self) -> bool {
            if !self.status.can_spawn() {
                return false;
            }
            let can_spawn = match &self.parent {
                Some(parent) => {
                    // If our parent already knows we have tasks, we can always
                    // spawn. Otherwise, we have to recurse.
                    self.subscopes_with_tasks > 0 || parent.lock().register_first_task()
                }
                None => true,
            };
            if can_spawn {
                self.subscopes_with_tasks += 1;
                debug_assert!(self.subscopes_with_tasks as usize <= self.children.len() + 1);
            };
            can_spawn
        }

        fn on_last_task_removed(
            this: &mut ConditionGuard<'_, ScopeState>,
            num_wakers_hint: usize,
            wakers: &mut Vec<Waker>,
        ) {
            debug_assert!(this.subscopes_with_tasks > 0);
            this.subscopes_with_tasks -= 1;
            if this.subscopes_with_tasks > 0 {
                wakers.reserve(num_wakers_hint);
                return;
            }

            match &this.parent {
                Some(parent) => {
                    Self::on_last_task_removed(
                        &mut parent.lock(),
                        num_wakers_hint + this.waker_count(),
                        wakers,
                    );
                }
                None => wakers.reserve(num_wakers_hint),
            };
            wakers.extend(this.drain_wakers());
        }
    }

    #[derive(Default)]
    struct WakeVec(Vec<Waker>);

    impl Drop for WakeVec {
        fn drop(&mut self) {
            for waker in self.0.drain(..) {
                waker.wake();
            }
        }
    }

    // WakeVec *must* come after the guard because we want the guard to be dropped first.
    pub struct ScopeWaker<'a>(ConditionGuard<'a, ScopeState>, WakeVec);

    impl<'a> From<ConditionGuard<'a, ScopeState>> for ScopeWaker<'a> {
        fn from(value: ConditionGuard<'a, ScopeState>) -> Self {
            Self(value, WakeVec::default())
        }
    }

    impl ScopeWaker<'_> {
        pub fn take_task(&mut self, id: usize) -> Option<TaskHandle> {
            let task = self.all_tasks.take(&id);
            if task.is_some() {
                self.on_task_removed(0);
            }
            task
        }

        pub fn task_did_finish(&mut self, id: usize) {
            if let Some(task) = self.all_tasks.take(&id) {
                self.on_task_removed(1);
                if !task.is_detached() {
                    let maybe_waker = self.results.task_did_finish(task);
                    self.1 .0.extend(maybe_waker);
                }
            }
        }

        pub fn set_closed_and_drain(
            &mut self,
        ) -> (HashSet<TaskHandle>, Box<dyn Any>, hash_set::Drain<'_, WeakScopeHandle>) {
            self.close();
            let all_tasks = std::mem::take(&mut self.all_tasks);
            let results = self.results.take();
            if !all_tasks.is_empty() {
                self.on_task_removed(0)
            }
            let children = self.children.drain();
            (all_tasks, results, children)
        }

        fn on_task_removed(&mut self, num_wakers_hint: usize) {
            if self.all_tasks.is_empty() {
                ScopeState::on_last_task_removed(&mut self.0, num_wakers_hint, &mut self.1 .0)
            }
        }
    }

    impl<'a> Deref for ScopeWaker<'a> {
        type Target = ConditionGuard<'a, ScopeState>;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl DerefMut for ScopeWaker<'_> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }
}

struct ScopeInner {
    executor: Arc<Executor>,
    state: Condition<ScopeState>,
    name: String,
}

impl Drop for ScopeInner {
    fn drop(&mut self) {
        // SAFETY: PtrKey is a ZST so we aren't creating a reference to invalid memory.
        // This also complies with the correctness requirements of
        // HashSet::remove because the implementations of Hash and Eq match
        // between PtrKey and WeakScopeHandle.
        let key = unsafe { &*(self as *const _ as *const PtrKey) };
        if let Some(parent) = &self.state.lock().parent {
            let mut parent_state = parent.lock();
            parent_state.remove_child(key);
        }
    }
}

impl ScopeHandle {
    fn with_current<R>(f: impl FnOnce(&ScopeHandle) -> R) -> R {
        super::common::TaskHandle::with_current(|task| match task {
            Some(task) => f(task.scope()),
            None => f(EHandle::local().global_scope()),
        })
    }

    fn lock(&self) -> ConditionGuard<'_, ScopeState> {
        self.inner.state.lock()
    }

    fn downgrade(&self) -> WeakScopeHandle {
        WeakScopeHandle { inner: Arc::downgrade(&self.inner) }
    }

    #[inline(always)]
    pub(crate) fn executor(&self) -> &Arc<Executor> {
        &self.inner.executor
    }

    /// Marks the task as detached.
    pub(crate) fn detach(&self, task_id: usize) {
        let _maybe_task = {
            let mut state = self.lock();
            if let Some(task) = state.all_tasks().get(&task_id) {
                task.detach();
            }
            state.results.detach(task_id)
        };
    }

    /// Cancels the task.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `R` is the correct type.
    pub(crate) unsafe fn cancel_task<R>(&self, task_id: usize) -> Option<R> {
        let mut state = self.lock();
        if let Some(task) = state.results.detach(task_id) {
            drop(state);
            return task.take_result();
        }
        state.all_tasks().get(&task_id).and_then(|task| {
            if task.cancel() {
                self.inner.executor.ready_tasks.push(task.clone());
            }
            task.take_result()
        })
    }

    /// Cancels and detaches the task.
    pub(crate) fn cancel_and_detach(&self, task_id: usize) {
        let _tasks = {
            let mut state = ScopeWaker::from(self.lock());
            let maybe_task1 = state.results.detach(task_id);
            let mut maybe_task2 = None;
            if let Some(task) = state.all_tasks().get(&task_id) {
                match task.cancel_and_detach() {
                    CancelAndDetachResult::Done => maybe_task2 = state.take_task(task_id),
                    CancelAndDetachResult::AddToRunQueue => {
                        self.inner.executor.ready_tasks.push(task.clone());
                    }
                    CancelAndDetachResult::Pending => {}
                }
            }
            (maybe_task1, maybe_task2)
        };
    }

    /// Polls for a join result for the given task ID.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `R` is the correct type.
    pub(crate) unsafe fn poll_join_result<R>(
        &self,
        task_id: usize,
        cx: &mut Context<'_>,
    ) -> Poll<R> {
        let task = ready!(self.lock().results.poll_join_result(task_id, cx));
        match task.take_result() {
            Some(result) => Poll::Ready(result),
            None => {
                // The task has been cancelled so all we can do is forever return pending.
                Poll::Pending
            }
        }
    }

    /// Polls for the task to be cancelled.
    pub(crate) unsafe fn poll_cancelled<R>(
        &self,
        task_id: usize,
        cx: &mut Context<'_>,
    ) -> Poll<Option<R>> {
        let task = self.lock().results.poll_join_result(task_id, cx);
        task.map(|task| task.take_result())
    }

    pub(super) fn insert_task(&self, task: TaskHandle, for_stream: bool) -> bool {
        let returned_task = self.lock().insert_task(task, for_stream);
        returned_task.is_none()
    }

    /// Drops the specified task.
    ///
    /// The main task by the single-threaded executor might not be 'static, so we use this to drop
    /// the task and make sure we meet lifetime guarantees.  Note that removing the task from our
    /// task list isn't sufficient; we must make sure the future running in the task is dropped.
    ///
    /// # Safety
    ///
    /// This is unsafe because of the call to `drop_future_unchecked` which requires that no
    /// thread is currently polling the task.
    pub(super) unsafe fn drop_task_unchecked(&self, task_id: usize) {
        let mut state = ScopeWaker::from(self.lock());
        let task = state.take_task(task_id);
        if let Some(task) = task {
            task.drop_future_unchecked();
        }
    }

    pub(super) fn task_did_finish(&self, id: usize) {
        let mut state = ScopeWaker::from(self.lock());
        state.task_did_finish(id);
    }

    /// Cancels tasks in this scope and all child scopes.
    fn cancel_all_tasks(&self) {
        let mut scopes = vec![self.clone()];
        while let Some(scope) = scopes.pop() {
            let mut state = scope.lock();
            if !state.status().might_have_running_tasks() {
                // Already cancelled or closed.
                continue;
            }
            for task in state.all_tasks() {
                if task.cancel() {
                    task.scope().executor().ready_tasks.push(task.clone());
                }
                // Don't bother dropping tasks that are finished; the entire
                // scope is going to be dropped soon anyway.
            }
            // Copy children to a vec so we don't hold the lock for too long.
            scopes.extend(state.children().iter().filter_map(|child| child.upgrade()));
            state.mark_finished();
        }
    }

    /// Drops tasks in this scope and all child scopes.
    ///
    /// # Panics
    ///
    /// Panics if any task is being accessed by another thread. Only call this
    /// method when the executor is shutting down and there are no other pollers.
    pub(super) fn drop_all_tasks(&self) {
        let mut scopes = vec![self.clone()];
        while let Some(scope) = scopes.pop() {
            let (tasks, join_results) = {
                let mut state = ScopeWaker::from(scope.lock());
                let (tasks, join_results, children) = state.set_closed_and_drain();
                scopes.extend(children.filter_map(|child| child.upgrade()));
                (tasks, join_results)
            };
            // Call task destructors once the scope lock is released so we don't risk a deadlock.
            for task in tasks {
                task.try_drop().expect("Expected drop to succeed");
            }
            std::mem::drop(join_results);
        }
    }
}

/// Optimizes removal from parent scope.
#[repr(transparent)]
struct PtrKey;

impl Borrow<PtrKey> for WeakScopeHandle {
    fn borrow(&self) -> &PtrKey {
        // SAFETY: PtrKey is a ZST so we aren't creating a reference to invalid memory.
        unsafe { &*(self.inner.as_ptr() as *const PtrKey) }
    }
}

impl PartialEq for PtrKey {
    fn eq(&self, other: &Self) -> bool {
        self as *const _ == other as *const _
    }
}

impl Eq for PtrKey {}

impl hash::Hash for PtrKey {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        (self as *const PtrKey).hash(state);
    }
}

#[derive(Default)]
struct JoinResults(HashMap<usize, JoinResult>);

trait Results: Send + Sync + 'static {
    /// Returns true if we allow spawning futures with arbitrary outputs on the scope.
    fn can_spawn(&self) -> bool;

    /// Polls for the specified task having finished.
    fn poll_join_result(&mut self, task_id: usize, cx: &mut Context<'_>) -> Poll<TaskHandle>;

    /// Called when a task finishes.
    fn task_did_finish(&mut self, task: TaskHandle) -> Option<Waker>;

    /// Called to drop any results for a particular task.
    fn detach(&mut self, task_id: usize) -> Option<TaskHandle>;

    /// Takes *all* the stored results.
    fn take(&mut self) -> Box<dyn Any>;

    /// Used only for testing.  Returns true if there are any results registered.
    #[cfg(test)]
    fn is_empty(&self) -> bool;
}

impl Results for JoinResults {
    fn can_spawn(&self) -> bool {
        true
    }

    fn poll_join_result(&mut self, task_id: usize, cx: &mut Context<'_>) -> Poll<TaskHandle> {
        match self.0.entry(task_id) {
            Entry::Occupied(mut o) => match o.get_mut() {
                JoinResult::Waker(waker) => *waker = cx.waker().clone(),
                JoinResult::Result(_) => {
                    let JoinResult::Result(task) = o.remove() else { unreachable!() };
                    return Poll::Ready(task);
                }
            },
            Entry::Vacant(v) => {
                v.insert(JoinResult::Waker(cx.waker().clone()));
            }
        }
        Poll::Pending
    }

    fn task_did_finish(&mut self, task: TaskHandle) -> Option<Waker> {
        match self.0.entry(task.id()) {
            Entry::Occupied(mut o) => {
                let JoinResult::Waker(waker) =
                    std::mem::replace(o.get_mut(), JoinResult::Result(task))
                else {
                    // It can't be JoinResult::Result because this function is the only
                    // function that sets that, and `task_did_finish` won't get called
                    // twice.
                    unreachable!()
                };
                Some(waker)
            }
            Entry::Vacant(v) => {
                v.insert(JoinResult::Result(task));
                None
            }
        }
    }

    fn detach(&mut self, task_id: usize) -> Option<TaskHandle> {
        match self.0.remove(&task_id) {
            Some(JoinResult::Result(task)) => Some(task),
            _ => None,
        }
    }

    fn take(&mut self) -> Box<dyn Any> {
        Box::new(Self(std::mem::take(&mut self.0)))
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Default)]
struct ResultsStream<R> {
    inner: Arc<Mutex<ResultsStreamInner<R>>>,
}

struct ResultsStreamInner<R> {
    results: Vec<R>,
    waker: Option<Waker>,
}

impl<R> Default for ResultsStreamInner<R> {
    fn default() -> Self {
        Self { results: Vec::new(), waker: None }
    }
}

impl<R: Send + 'static> Results for ResultsStream<R> {
    fn can_spawn(&self) -> bool {
        false
    }

    fn poll_join_result(&mut self, _task_id: usize, _cx: &mut Context<'_>) -> Poll<TaskHandle> {
        Poll::Pending
    }

    fn task_did_finish(&mut self, task: TaskHandle) -> Option<Waker> {
        let mut inner = self.inner.lock();
        // SAFETY: R is guaranteed to be the same return type as all futures finishing on this
        // scope.
        inner.results.extend(unsafe { task.take_result() });
        inner.waker.take()
    }

    fn detach(&mut self, _task_id: usize) -> Option<TaskHandle> {
        None
    }

    fn take(&mut self) -> Box<dyn Any> {
        Box::new(std::mem::take(&mut self.inner.lock().results))
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EHandle, LocalExecutor, SendExecutor, SpawnableFuture, Task, TestExecutor, Timer};
    use assert_matches::assert_matches;
    use fuchsia_sync::{Condvar, Mutex};
    use futures::channel::mpsc;
    use futures::future::join_all;
    use futures::{FutureExt, StreamExt};
    use std::future::{pending, poll_fn};
    use std::pin::{pin, Pin};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    #[derive(Default)]
    struct RemoteControlFuture(Mutex<RCFState>);
    #[derive(Default)]
    struct RCFState {
        resolved: bool,
        waker: Option<Waker>,
    }

    impl Future for &RemoteControlFuture {
        type Output = ();
        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            let mut this = self.0.lock();
            if this.resolved {
                Poll::Ready(())
            } else {
                this.waker.replace(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    impl RemoteControlFuture {
        fn new() -> Arc<Self> {
            Arc::new(Default::default())
        }

        fn resolve(&self) {
            let mut this = self.0.lock();
            this.resolved = true;
            if let Some(waker) = this.waker.take() {
                waker.wake();
            }
        }

        fn as_future(self: &Arc<Self>) -> impl Future<Output = ()> {
            let this = Arc::clone(self);
            async move { (&*this).await }
        }
    }

    #[test]
    fn compute_works_on_root_scope() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope();
        let mut task = pin!(scope.compute(async { 1 }));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Ready(1));
    }

    #[test]
    fn compute_works_on_new_child() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child_with_name("compute_works_on_new_child");
        let mut task = pin!(scope.compute(async { 1 }));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Ready(1));
    }

    #[test]
    fn scope_drop_cancels_tasks() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child_with_name("scope_drop_cancels_tasks");
        let mut task = pin!(scope.compute(async { 1 }));
        drop(scope);
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Pending);
    }

    #[test]
    fn tasks_do_not_spawn_on_cancelled_scopes() {
        let mut executor = TestExecutor::new();
        let scope =
            executor.global_scope().new_child_with_name("tasks_do_not_spawn_on_cancelled_scopes");
        let handle = scope.to_handle();
        let mut cancel = pin!(scope.cancel());
        assert_eq!(executor.run_until_stalled(&mut cancel), Poll::Ready(()));
        let mut task = pin!(handle.compute(async { 1 }));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Pending);
    }

    #[test]
    fn tasks_do_not_spawn_on_closed_empty_scopes() {
        let mut executor = TestExecutor::new();
        let scope =
            executor.global_scope().new_child_with_name("tasks_do_not_spawn_closed_empty_scopes");
        let handle = scope.to_handle();
        let mut close = pin!(scope.cancel());
        assert_eq!(executor.run_until_stalled(&mut close), Poll::Ready(()));
        let mut task = pin!(handle.compute(async { 1 }));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Pending);
    }

    #[test]
    fn tasks_do_not_spawn_on_closed_nonempty_scopes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        handle.spawn(pending());
        let mut close = pin!(scope.close());
        assert_eq!(executor.run_until_stalled(&mut close), Poll::Pending);
        let mut task = pin!(handle.compute(async { 1 }));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Pending);
    }

    #[test]
    fn spawn_works_on_child_and_grandchild() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        let grandchild = child.new_child();
        let mut child_task = pin!(child.compute(async { 1 }));
        let mut grandchild_task = pin!(grandchild.compute(async { 1 }));
        assert_eq!(executor.run_until_stalled(&mut child_task), Poll::Ready(1));
        assert_eq!(executor.run_until_stalled(&mut grandchild_task), Poll::Ready(1));
    }

    #[test]
    fn spawn_drop_cancels_child_and_grandchild_tasks() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        let grandchild = child.new_child();
        let mut child_task = pin!(child.compute(async { 1 }));
        let mut grandchild_task = pin!(grandchild.compute(async { 1 }));
        drop(scope);
        assert_eq!(executor.run_until_stalled(&mut child_task), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut grandchild_task), Poll::Pending);
    }

    #[test]
    fn completed_tasks_are_cleaned_up_after_cancel() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();

        let task1 = scope.spawn(pending::<()>());
        let task2 = scope.spawn(async {});
        assert_eq!(executor.run_until_stalled(&mut pending::<()>()), Poll::Pending);
        assert_eq!(scope.lock().all_tasks().len(), 1);

        // Running the executor after cancelling the task isn't currently
        // necessary, but we might decide to do async cleanup in the future.
        assert_eq!(task1.cancel().now_or_never(), None);
        assert_eq!(task2.cancel().now_or_never(), Some(Some(())));

        assert_eq!(executor.run_until_stalled(&mut pending::<()>()), Poll::Pending);
        assert_eq!(scope.lock().all_tasks().len(), 0);
        assert!(scope.lock().results.is_empty());
    }

    #[test]
    fn join_emtpy_scope() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        assert_eq!(executor.run_until_stalled(&mut pin!(scope.join())), Poll::Ready(()));
    }

    #[test]
    fn task_handle_preserves_access_to_result_after_join_begins() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let mut task = scope.compute(async { 1 });
        scope.spawn(async {});
        let task2 = scope.spawn(pending::<()>());
        // Fuse to stay agnostic as to whether the join completes before or
        // after awaiting the task handle.
        let mut join = pin!(scope.join().fuse());
        let _ = executor.run_until_stalled(&mut join);
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Ready(1));
        let _ = task2.cancel();
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Ready(()));
    }

    #[test]
    fn join_blocks_until_task_is_cancelled() {
        // Scope with one outstanding task handle and one cancelled task.
        // The scope is not complete until the outstanding task handle is cancelled.
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let outstanding_task = scope.spawn(pending::<()>());
        let cancelled_task = scope.spawn(pending::<()>());
        assert_eq!(
            executor.run_until_stalled(&mut pin!(cancelled_task.cancel())),
            Poll::Ready(None)
        );
        let mut join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Pending);
        assert_eq!(
            executor.run_until_stalled(&mut pin!(outstanding_task.cancel())),
            Poll::Ready(None)
        );
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Ready(()));
    }

    #[test]
    fn join_blocks_but_cancel_succeeds_if_detached_task_never_completes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        // The default is to detach.
        scope.spawn(pending::<()>());
        let mut join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Pending);
        let mut cancel = pin!(join.cancel());
        assert_eq!(executor.run_until_stalled(&mut cancel), Poll::Ready(()));
    }

    #[test]
    fn close_blocks_but_cancel_succeeds_if_detached_task_never_completes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        // The default is to detach.
        scope.spawn(pending::<()>());
        let mut close = pin!(scope.close());
        assert_eq!(executor.run_until_stalled(&mut close), Poll::Pending);
        let mut cancel = pin!(close.cancel());
        assert_eq!(executor.run_until_stalled(&mut cancel), Poll::Ready(()));
    }

    #[test]
    fn join_scope_blocks_until_spawned_task_completes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let remote = RemoteControlFuture::new();
        let mut task = scope.spawn(remote.as_future());
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
        remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Ready(()));
    }

    #[test]
    fn close_scope_blocks_until_spawned_task_completes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let remote = RemoteControlFuture::new();
        let mut task = scope.spawn(remote.as_future());
        let mut scope_close = pin!(scope.close());
        assert_eq!(executor.run_until_stalled(&mut scope_close), Poll::Pending);
        remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut scope_close), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Ready(()));
    }

    #[test]
    fn join_scope_blocks_until_detached_task_of_detached_child_completes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        let remote = RemoteControlFuture::new();
        child.spawn(remote.as_future());
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut pin!(child.on_no_tasks())), Poll::Pending);
        child.detach();
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
        remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Ready(()));
    }

    #[test]
    fn join_scope_blocks_until_task_spawned_from_nested_detached_scope_completes() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let remote = RemoteControlFuture::new();
        {
            let remote = remote.clone();
            scope.spawn(async move {
                let child = Scope::new_with_name("child");
                child.spawn(async move {
                    Scope::current().spawn(remote.as_future());
                });
                child.detach();
            });
        }
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
        remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Ready(()));
    }

    #[test]
    fn join_scope_blocks_when_blocked_child_is_detached() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        child.spawn(pending());
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut pin!(child.on_no_tasks())), Poll::Pending);
        child.detach();
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
    }

    #[test]
    fn join_scope_completes_when_blocked_child_is_cancelled() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        child.spawn(pending());
        let mut scope_join = pin!(scope.join());
        {
            let mut child_join = pin!(child.join());
            assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
            assert_eq!(executor.run_until_stalled(&mut child_join), Poll::Pending);
        }
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Ready(()));
    }

    #[test]
    fn detached_scope_can_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        scope.detach();
        assert_eq!(executor.run_until_stalled(&mut handle.compute(async { 1 })), Poll::Ready(1));
    }

    #[test]
    fn dropped_scope_cannot_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        drop(scope);
        assert_eq!(executor.run_until_stalled(&mut handle.compute(async { 1 })), Poll::Pending);
    }

    #[test]
    fn dropped_scope_with_running_task_cannot_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        let _running_task = handle.spawn(pending::<()>());
        drop(scope);
        assert_eq!(executor.run_until_stalled(&mut handle.compute(async { 1 })), Poll::Pending);
    }

    #[test]
    fn joined_scope_cannot_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut handle.compute(async { 1 })), Poll::Pending);
    }

    #[test]
    fn joining_scope_with_running_task_can_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        let _running_task = handle.spawn(pending::<()>());
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut handle.compute(async { 1 })), Poll::Ready(1));
    }

    #[test]
    fn joined_scope_child_cannot_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        let child_before_join = scope.new_child();
        assert_eq!(
            executor.run_until_stalled(&mut child_before_join.compute(async { 1 })),
            Poll::Ready(1)
        );
        let mut scope_join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut scope_join), Poll::Ready(()));
        let child_after_join = handle.new_child();
        let grandchild_after_join = child_before_join.new_child();
        assert_eq!(
            executor.run_until_stalled(&mut child_before_join.compute(async { 1 })),
            Poll::Pending
        );
        assert_eq!(
            executor.run_until_stalled(&mut child_after_join.compute(async { 1 })),
            Poll::Pending
        );
        assert_eq!(
            executor.run_until_stalled(&mut grandchild_after_join.compute(async { 1 })),
            Poll::Pending
        );
    }

    #[test]
    fn closed_scope_child_cannot_spawn() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        let child_before_close = scope.new_child();
        assert_eq!(
            executor.run_until_stalled(&mut child_before_close.compute(async { 1 })),
            Poll::Ready(1)
        );
        let mut scope_close = pin!(scope.close());
        assert_eq!(executor.run_until_stalled(&mut scope_close), Poll::Ready(()));
        let child_after_close = handle.new_child();
        let grandchild_after_close = child_before_close.new_child();
        assert_eq!(
            executor.run_until_stalled(&mut child_before_close.compute(async { 1 })),
            Poll::Pending
        );
        assert_eq!(
            executor.run_until_stalled(&mut child_after_close.compute(async { 1 })),
            Poll::Pending
        );
        assert_eq!(
            executor.run_until_stalled(&mut grandchild_after_close.compute(async { 1 })),
            Poll::Pending
        );
    }

    #[test]
    fn can_join_child_first() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        assert_eq!(executor.run_until_stalled(&mut child.compute(async { 1 })), Poll::Ready(1));
        assert_eq!(executor.run_until_stalled(&mut pin!(child.join())), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut pin!(scope.join())), Poll::Ready(()));
    }

    #[test]
    fn can_join_parent_first() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        assert_eq!(executor.run_until_stalled(&mut child.compute(async { 1 })), Poll::Ready(1));
        assert_eq!(executor.run_until_stalled(&mut pin!(scope.join())), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut pin!(child.join())), Poll::Ready(()));
    }

    #[test]
    fn task_in_parent_scope_can_join_child() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let child = scope.new_child();
        let remote = RemoteControlFuture::new();
        child.spawn(remote.as_future());
        scope.spawn(async move { child.join().await });
        let mut join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Pending);
        remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Ready(()));
    }

    #[test]
    fn join_completes_while_completed_task_handle_is_held() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let mut task = scope.compute(async { 1 });
        scope.spawn(async {});
        let mut join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Ready(1));
    }

    #[test]
    fn cancel_completes_while_task_holds_handle() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let handle = scope.to_handle();
        let mut task = scope.compute(async move {
            loop {
                pending::<()>().await; // never returns
                handle.spawn(async {});
            }
        });

        // Join should not complete because the task never does.
        let mut join = pin!(scope.join());
        assert_eq!(executor.run_until_stalled(&mut join), Poll::Pending);

        let mut cancel = pin!(join.cancel());
        assert_eq!(executor.run_until_stalled(&mut cancel), Poll::Ready(()));
        assert_eq!(executor.run_until_stalled(&mut task), Poll::Pending);
    }

    #[test]
    fn cancel_from_handle_inside_task() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        {
            // Spawn a task that never finishes until the scope is cancelled.
            scope.spawn(pending::<()>());

            let mut no_tasks = pin!(scope.on_no_tasks());
            assert_eq!(executor.run_until_stalled(&mut no_tasks), Poll::Pending);

            let handle = scope.to_handle();
            scope.spawn(async move {
                handle.cancel().await;
                panic!("cancel() should never complete");
            });

            assert_eq!(executor.run_until_stalled(&mut no_tasks), Poll::Ready(()));
        }
        assert_eq!(scope.join().now_or_never(), Some(()));
    }

    #[test]
    fn can_spawn_from_non_executor_thread() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().clone();
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        let _ = std::thread::spawn(move || {
            scope.spawn(async move {
                done_clone.store(true, Ordering::Relaxed);
            })
        })
        .join();
        let _ = executor.run_until_stalled(&mut pending::<()>());
        assert!(done.load(Ordering::Relaxed));
    }

    #[test]
    fn scope_tree() {
        // A
        //  \
        //   B
        //  / \
        // C   D
        let mut executor = TestExecutor::new();
        let a = executor.global_scope().new_child();
        let b = a.new_child();
        let c = b.new_child();
        let d = b.new_child();
        let a_remote = RemoteControlFuture::new();
        let c_remote = RemoteControlFuture::new();
        let d_remote = RemoteControlFuture::new();
        a.spawn(a_remote.as_future());
        c.spawn(c_remote.as_future());
        d.spawn(d_remote.as_future());
        let mut a_join = pin!(a.join());
        let mut b_join = pin!(b.join());
        let mut d_join = pin!(d.join());
        assert_eq!(executor.run_until_stalled(&mut a_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut b_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut d_join), Poll::Pending);
        d_remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut a_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut b_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut d_join), Poll::Ready(()));
        c_remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut a_join), Poll::Pending);
        assert_eq!(executor.run_until_stalled(&mut b_join), Poll::Ready(()));
        a_remote.resolve();
        assert_eq!(executor.run_until_stalled(&mut a_join), Poll::Ready(()));
        let mut c_join = pin!(c.join());
        assert_eq!(executor.run_until_stalled(&mut c_join), Poll::Ready(()));
    }

    #[test]
    fn on_no_tasks() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();
        let _task1 = scope.spawn(std::future::ready(()));
        let task2 = scope.spawn(pending::<()>());

        let mut on_no_tasks = pin!(scope.on_no_tasks());

        assert!(executor.run_until_stalled(&mut on_no_tasks).is_pending());

        let _ = task2.cancel();

        let on_no_tasks2 = pin!(scope.on_no_tasks());
        let on_no_tasks3 = pin!(scope.on_no_tasks());

        assert_matches!(
            executor.run_until_stalled(&mut join_all([on_no_tasks, on_no_tasks2, on_no_tasks3])),
            Poll::Ready(_)
        );
    }

    #[test]
    fn wake_all() {
        let mut executor = TestExecutor::new();
        let scope = executor.global_scope().new_child();

        let poll_count = Arc::new(AtomicU64::new(0));

        struct PollCounter(Arc<AtomicU64>);

        impl Future for PollCounter {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                self.0.fetch_add(1, Ordering::Relaxed);
                Poll::Pending
            }
        }

        scope.spawn(PollCounter(poll_count.clone()));
        scope.spawn(PollCounter(poll_count.clone()));

        let _ = executor.run_until_stalled(&mut pending::<()>());

        let mut start_count = poll_count.load(Ordering::Relaxed);

        for _ in 0..2 {
            scope.wake_all();
            let _ = executor.run_until_stalled(&mut pending::<()>());
            assert_eq!(poll_count.load(Ordering::Relaxed), start_count + 2);
            start_count += 2;
        }
    }

    #[test]
    fn on_no_tasks_race() {
        fn sleep_random() {
            use rand::Rng;
            std::thread::sleep(std::time::Duration::from_micros(
                rand::thread_rng().gen_range(0..10),
            ));
        }
        for _ in 0..2000 {
            let mut executor = SendExecutor::new(2);
            let scope = executor.root_scope().new_child();
            scope.spawn(async {
                sleep_random();
            });
            executor.run(async move {
                sleep_random();
                scope.on_no_tasks().await;
            });
        }
    }

    async fn yield_to_executor() {
        let mut done = false;
        poll_fn(|cx| {
            if done {
                Poll::Ready(())
            } else {
                done = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    #[test]
    fn test_detach() {
        let mut e = LocalExecutor::new();
        e.run_singlethreaded(async {
            let counter = Arc::new(AtomicU32::new(0));

            {
                let counter = counter.clone();
                Task::spawn(async move {
                    for _ in 0..5 {
                        yield_to_executor().await;
                        counter.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .detach();
            }

            while counter.load(Ordering::Relaxed) != 5 {
                yield_to_executor().await;
            }
        });

        assert!(e.ehandle.root_scope.lock().results.is_empty());
    }

    #[test]
    fn test_cancel() {
        let mut e = LocalExecutor::new();
        e.run_singlethreaded(async {
            let ref_count = Arc::new(());
            // First, just drop the task.
            {
                let ref_count = ref_count.clone();
                let _ = Task::spawn(async move {
                    let _ref_count = ref_count;
                    let _: () = std::future::pending().await;
                });
            }

            while Arc::strong_count(&ref_count) != 1 {
                yield_to_executor().await;
            }

            // Now try explicitly cancelling.
            let task = {
                let ref_count = ref_count.clone();
                Task::spawn(async move {
                    let _ref_count = ref_count;
                    let _: () = std::future::pending().await;
                })
            };

            assert_eq!(task.cancel().await, None);
            while Arc::strong_count(&ref_count) != 1 {
                yield_to_executor().await;
            }

            // Now cancel a task that has already finished.
            let task = {
                let ref_count = ref_count.clone();
                Task::spawn(async move {
                    let _ref_count = ref_count;
                })
            };

            // Wait for it to finish.
            while Arc::strong_count(&ref_count) != 1 {
                yield_to_executor().await;
            }

            assert_eq!(task.cancel().await, Some(()));
        });

        assert!(e.ehandle.root_scope.lock().results.is_empty());
    }

    #[test]
    fn test_cancel_waits() {
        let mut executor = SendExecutor::new(2);
        let running = Arc::new((Mutex::new(false), Condvar::new()));
        let task = {
            let running = running.clone();
            executor.root_scope().compute(async move {
                *running.0.lock() = true;
                running.1.notify_all();
                std::thread::sleep(std::time::Duration::from_millis(10));
                *running.0.lock() = false;
                "foo"
            })
        };
        executor.run(async move {
            {
                let mut guard = running.0.lock();
                while !*guard {
                    running.1.wait(&mut guard);
                }
            }
            assert_eq!(task.cancel().await, Some("foo"));
            assert!(!*running.0.lock());
        });
    }

    fn test_clean_up(callback: impl FnOnce(Task<()>) + Send + 'static) {
        let mut executor = SendExecutor::new(2);
        let running = Arc::new((Mutex::new(false), Condvar::new()));
        let can_quit = Arc::new((Mutex::new(false), Condvar::new()));
        let task = {
            let running = running.clone();
            let can_quit = can_quit.clone();
            executor.root_scope().compute(async move {
                *running.0.lock() = true;
                running.1.notify_all();
                {
                    let mut guard = can_quit.0.lock();
                    while !*guard {
                        can_quit.1.wait(&mut guard);
                    }
                }
                *running.0.lock() = false;
            })
        };
        executor.run(async move {
            {
                let mut guard = running.0.lock();
                while !*guard {
                    running.1.wait(&mut guard);
                }
            }

            callback(task);

            *can_quit.0.lock() = true;
            can_quit.1.notify_all();

            let ehandle = EHandle::local();
            let scope = ehandle.global_scope();

            // The only way of testing for this is to poll.
            while scope.lock().all_tasks().len() > 1 || !scope.lock().results.is_empty() {
                Timer::new(std::time::Duration::from_millis(1)).await;
            }

            assert!(!*running.0.lock());
        });
    }

    #[test]
    fn test_dropped_cancel_cleans_up() {
        test_clean_up(|task| {
            let cancel_fut = std::pin::pin!(task.cancel());
            let waker = futures::task::noop_waker();
            assert!(cancel_fut.poll(&mut Context::from_waker(&waker)).is_pending());
        });
    }

    #[test]
    fn test_dropped_task_cleans_up() {
        test_clean_up(|task| {
            std::mem::drop(task);
        });
    }

    #[test]
    fn test_detach_cleans_up() {
        test_clean_up(|task| {
            task.detach();
        });
    }

    #[test]
    fn test_scope_stream() {
        let mut executor = SendExecutor::new(2);
        executor.run(async move {
            let (stream, handle) = ScopeStream::new();
            handle.push(async { 1 });
            handle.push(async { 2 });
            stream.close();
            let results: HashSet<_> = stream.collect().await;
            assert_eq!(results, HashSet::from_iter([1, 2]));
        });
    }

    #[test]
    fn test_scope_stream_wakes_properly() {
        let mut executor = SendExecutor::new(2);
        executor.run(async move {
            let (stream, handle) = ScopeStream::new();
            handle.push(async {
                Timer::new(Duration::from_millis(10)).await;
                1
            });
            handle.push(async {
                Timer::new(Duration::from_millis(10)).await;
                2
            });
            stream.close();
            let results: HashSet<_> = stream.collect().await;
            assert_eq!(results, HashSet::from_iter([1, 2]));
        });
    }

    #[test]
    fn test_scope_stream_drops_spawned_tasks() {
        let mut executor = SendExecutor::new(2);
        executor.run(async move {
            let (stream, handle) = ScopeStream::new();
            handle.push(async { 1 });
            let _task = stream.compute(async { "foo" });
            stream.close();
            let results: HashSet<_> = stream.collect().await;
            assert_eq!(results, HashSet::from_iter([1]));
        });
    }

    #[test]
    fn test_nested_scope_stream() {
        let mut executor = SendExecutor::new(2);
        executor.run(async move {
            let (mut stream, handle) = ScopeStream::new();
            handle.clone().push(async move {
                handle.clone().push(async move {
                    handle.clone().push(async move { 3 });
                    2
                });
                1
            });
            let mut results = HashSet::default();
            while let Some(item) = stream.next().await {
                results.insert(item);
                if results.len() == 3 {
                    stream.close();
                }
            }
            assert_eq!(results, HashSet::from_iter([1, 2, 3]));
        });
    }

    #[test]
    fn test_dropping_scope_stream_cancels_all_tasks() {
        let mut executor = SendExecutor::new(2);
        executor.run(async move {
            let (stream, handle) = ScopeStream::new();
            let (tx1, mut rx) = mpsc::unbounded::<()>();
            let tx2 = tx1.clone();
            handle.push(async move {
                let _tx1 = tx1;
                let () = pending().await;
            });
            handle.push(async move {
                let _tx2 = tx2;
                let () = pending().await;
            });
            drop(stream);

            // This will wait forever if the tasks aren't cancelled.
            assert_eq!(rx.next().await, None);
        });
    }

    #[test]
    fn test_scope_stream_collect() {
        let mut executor = SendExecutor::new(2);
        executor.run(async move {
            let stream: ScopeStream<_> = (0..10).into_iter().map(|i| async move { i }).collect();
            assert_eq!(stream.collect::<HashSet<u32>>().await, HashSet::from_iter(0..10));

            let stream: ScopeStream<_> =
                (0..10).into_iter().map(|i| SpawnableFuture::new(async move { i })).collect();
            assert_eq!(stream.collect::<HashSet<u32>>().await, HashSet::from_iter(0..10));
        });
    }
}
