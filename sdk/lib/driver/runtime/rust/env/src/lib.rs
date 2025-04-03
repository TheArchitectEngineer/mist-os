// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe bindings for driver runtime environment.

#![deny(missing_docs)]

use fdf_sys::*;

use core::ffi;
use core::marker::PhantomData;
use core::ptr::{null_mut, NonNull};

use zx::Status;

use fdf::{Dispatcher, DispatcherBuilder, DispatcherRef, ShutdownObserver};

pub mod test;

/// Create the dispatcher as configured by this object. This must be called from a
/// thread managed by the driver runtime. The dispatcher returned is owned by the caller,
/// and will initiate asynchronous shutdown when the object is dropped unless
/// [`Dispatcher::release`] is called on it to convert it into an unowned [`DispatcherRef`].
///
fn create_with_driver<'a>(
    dispatcher: DispatcherBuilder,
    driver: DriverRefTypeErased<'a>,
) -> Result<Dispatcher, Status> {
    let mut out_dispatcher = null_mut();
    let owner = driver.0;
    let options = dispatcher.options;
    let name = dispatcher.name.as_ptr() as *mut ffi::c_char;
    let name_len = dispatcher.name.len();
    let scheduler_role = dispatcher.scheduler_role.as_ptr() as *mut ffi::c_char;
    let scheduler_role_len = dispatcher.scheduler_role.len();
    let observer =
        dispatcher.shutdown_observer.unwrap_or_else(|| ShutdownObserver::new(|_| {})).into_ptr();
    // SAFETY: all arguments point to memory that will be available for the duration
    // of the call, except `observer`, which will be available until it is unallocated
    // by the dispatcher exit handler.
    Status::ok(unsafe {
        fdf_env_dispatcher_create_with_owner(
            owner,
            options,
            name,
            name_len,
            scheduler_role,
            scheduler_role_len,
            observer,
            &mut out_dispatcher,
        )
    })?;
    // SAFETY: `out_dispatcher` is valid by construction if `fdf_dispatcher_create` returns
    // ZX_OK.
    Ok(unsafe { Dispatcher::from_raw(NonNull::new_unchecked(out_dispatcher)) })
}

/// As with [`create_with_driver`], this creates a new dispatcher as configured by this object, but
/// instead of returning an owned reference it immediately releases the reference to be
/// managed by the driver runtime.
///
/// # Safety
///
/// |owner| must outlive the dispatcher. You can use the shutdown_observer to find out when it is
/// safe to drop it.
fn create_with_driver_released<'a>(
    dispatcher: DispatcherBuilder,
    driver: DriverRefTypeErased<'a>,
) -> Result<DispatcherRef<'static>, Status> {
    create_with_driver(dispatcher, driver).map(Dispatcher::release)
}

/// A marker trait for a function that can be used as a driver shutdown observer with
/// [`Driver::shutdown`].
pub trait DriverShutdownObserverFn<T: 'static>:
    FnOnce(DriverRef<'static, T>) + Send + Sync + 'static
{
}
impl<T, U: 'static> DriverShutdownObserverFn<U> for T where
    T: FnOnce(DriverRef<'static, U>) + Send + Sync + 'static
{
}

/// A shutdown observer for [`fdf_dispatcher_create`] that can call any kind of callback instead of
/// just a C-compatible function when a dispatcher is shutdown.
///
/// # Safety
///
/// This object relies on a specific layout to allow it to be cast between a
/// `*mut fdf_dispatcher_shutdown_observer` and a `*mut ShutdownObserver`. To that end,
/// it is important that this struct stay both `#[repr(C)]` and that `observer` be its first member.
#[repr(C)]
struct DriverShutdownObserver<T: 'static> {
    observer: fdf_env_driver_shutdown_observer,
    shutdown_fn: Box<dyn DriverShutdownObserverFn<T>>,
    driver: Driver<T>,
}

impl<T: 'static> DriverShutdownObserver<T> {
    /// Creates a new [`ShutdownObserver`] with `f` as the callback to run when a dispatcher
    /// finishes shutting down.
    fn new<F: DriverShutdownObserverFn<T>>(driver: Driver<T>, f: F) -> Self {
        let shutdown_fn = Box::new(f);
        Self {
            observer: fdf_env_driver_shutdown_observer { handler: Some(Self::handler) },
            shutdown_fn,
            driver,
        }
    }

    /// Begins the driver shutdown procedure.
    /// Turns this object into a stable pointer suitable for passing to
    /// [`fdf_env_shutdown_dispatchers_async`] by wrapping it in a [`Box`] and leaking it
    /// to be reconstituded by [`Self::handler`] when the dispatcher is shut down.
    fn begin(self) -> Result<(), Status> {
        let driver = self.driver.inner.as_ptr() as *const _;
        // Note: this relies on the assumption that `self.observer` is at the beginning of the
        // struct.
        let this = Box::into_raw(Box::new(self)) as *mut _;
        // SAFTEY: driver is owned by the driver framework and will be kept alive until the handler
        // callback is triggered
        if let Err(e) = Status::ok(unsafe { fdf_env_shutdown_dispatchers_async(driver, this) }) {
            // SAFTEY: The framework didn't actually take ownership of the object if the call
            // fails, so we can recover it to avoid leaking.
            let _ = unsafe { Box::from_raw(this as *mut DriverShutdownObserver<T>) };
            return Err(e);
        }
        Ok(())
    }

    /// The callback that is registered with the driver that will be called when the driver
    /// is shut down.
    ///
    /// # Safety
    ///
    /// This function should only ever be called by the driver runtime at dispatcher shutdown
    /// time, must only ever be called once for any given [`ShutdownObserver`] object, and
    /// that [`ShutdownObserver`] object must have previously been made into a pointer by
    /// [`Self::into_ptr`].
    unsafe extern "C" fn handler(
        driver: *const ffi::c_void,
        observer: *mut fdf_env_driver_shutdown_observer_t,
    ) {
        // SAFETY: The driver framework promises to only call this function once, so we can
        // safely take ownership of the [`Box`] and deallocate it when this function ends.
        let observer = unsafe { Box::from_raw(observer as *mut DriverShutdownObserver<T>) };
        (observer.shutdown_fn)(DriverRef(driver as *const T, PhantomData));
    }
}

/// An owned handle to a Driver instance that can be used to create initial dispatchers.
#[derive(Debug)]
pub struct Driver<T> {
    pub(crate) inner: NonNull<T>,
    shutdown_triggered: bool,
}

/// An unowned handle to the driver that is returned through certain environment APIs like
/// |get_driver_on_thread_koid|.
pub struct UnownedDriver {
    inner: *const ffi::c_void,
}

/// SAFETY: This inner pointer is movable across threads.
unsafe impl<T: Send> Send for Driver<T> {}

impl<T: 'static> Driver<T> {
    /// Returns a builder capable of creating a new dispatcher. Note that this dispatcher cannot
    /// outlive the driver and is only capable of being stopped by shutting down the driver. It is
    /// meant to be created to serve as the initial or default dispatcher for a driver.
    pub fn new_dispatcher(
        &self,
        dispatcher: DispatcherBuilder,
    ) -> Result<DispatcherRef<'static>, Status> {
        create_with_driver_released(dispatcher, self.as_ref_type_erased())
    }

    /// Run a closure in the context of a driver.
    pub fn enter<R>(&mut self, f: impl FnOnce() -> R) -> R {
        unsafe { fdf_env_register_driver_entry(self.inner.as_ptr() as *const _) };
        let res = f();
        unsafe { fdf_env_register_driver_exit() };
        res
    }

    /// Adds an allowed scheduler role to the driver
    pub fn add_allowed_scheduler_role(&self, scheduler_role: &str) {
        let driver_ptr = self.inner.as_ptr() as *const _;
        let scheduler_role_ptr = scheduler_role.as_ptr() as *mut ffi::c_char;
        let scheduler_role_len = scheduler_role.len();
        unsafe {
            fdf_env_add_allowed_scheduler_role_for_driver(
                driver_ptr,
                scheduler_role_ptr,
                scheduler_role_len,
            )
        };
    }

    /// Asynchronously shuts down all dispatchers owned by |driver|.
    /// |f| will be called once shutdown completes. This is guaranteed to be
    /// after all the dispatcher's shutdown observers have been called, and will be running
    /// on the thread of the final dispatcher which has been shutdown.
    pub fn shutdown<F: DriverShutdownObserverFn<T>>(mut self, f: F) {
        self.shutdown_triggered = true;
        // It should be impossible for this to fail as we ensure we are the only caller of this
        // API, so it cannot be triggered twice nor before the driver has been registered with the
        // framework.
        DriverShutdownObserver::new(self, f)
            .begin()
            .expect("Unexpectedly failed start shutdown procedure")
    }

    /// Create a reference to a driver without ownership. The returned reference lacks the ability
    /// to perform most actions available to the owner of the driver, therefore it doesn't need to
    /// have it's lifetime tracked closely.
    fn as_ref_type_erased<'a>(&'a self) -> DriverRefTypeErased<'a> {
        DriverRefTypeErased(self.inner.as_ptr() as *const _, PhantomData)
    }

    /// Releases ownership of this driver instance, allowing it to be shut down when the runtime
    /// shuts down.
    pub fn release(self) -> DriverRef<'static, T> {
        DriverRef(self.inner.as_ptr() as *const _, PhantomData)
    }
}

impl<T> Drop for Driver<T> {
    fn drop(&mut self) {
        assert!(self.shutdown_triggered, "Cannot drop driver, must call shutdown method instead");
    }
}

impl<T> PartialEq<UnownedDriver> for Driver<T> {
    fn eq(&self, other: &UnownedDriver) -> bool {
        return self.inner.as_ptr() as *const _ == other.inner;
    }
}

// Note that inner type is not guaranteed to not be null.
#[derive(Clone, Copy, PartialEq)]
struct DriverRefTypeErased<'a>(*const ffi::c_void, PhantomData<&'a u32>);

impl Default for DriverRefTypeErased<'_> {
    fn default() -> Self {
        DriverRefTypeErased(std::ptr::null(), PhantomData)
    }
}

/// A lifetime-bound reference to a driver handle.
pub struct DriverRef<'a, T>(pub *const T, PhantomData<&'a Driver<T>>);

/// The driver runtime environment
pub struct Environment;

impl Environment {
    /// Whether the environment should enforce scheduler roles. Used with [`Self::start`].
    pub const ENFORCE_ALLOWED_SCHEDULER_ROLES: u32 = 1;

    /// Start the driver runtime. This sets up the initial thread that the dispatchers run on.
    pub fn start(options: u32) -> Result<Environment, Status> {
        // SAFETY: calling fdf_env_start, which does not have any soundness
        // concerns for rust code. It may be called multiple times without any problems.
        Status::ok(unsafe { fdf_env_start(options) })?;
        Ok(Self)
    }

    /// Creates a new driver. It is expected that the driver passed in is a leaked pointer which
    /// will only be recovered by triggering the shutdown method on the driver.
    ///
    /// # Panics
    ///
    /// This method will panic if |driver| is not null.
    pub fn new_driver<T>(&self, driver: *const T) -> Driver<T> {
        // We cast to *mut because there is not equivlaent version of NonNull for *const T.
        Driver {
            inner: NonNull::new(driver as *mut _).expect("driver must not be null"),
            shutdown_triggered: false,
        }
    }

    // TODO: Consider tracking all drivers and providing a method to shutdown all outstanding
    // drivers and block until they've all finished shutting down.

    /// Returns whether the current thread is managed by the driver runtime or not.
    fn current_thread_managed_by_driver_runtime() -> bool {
        // Safety: Calling fdf_dispatcher_get_current_dispatcher from any thread is safe. Because
        // we are not actually using the dispatcher, we don't need to worry about it's lifetime.
        !unsafe { fdf_dispatcher_get_current_dispatcher().is_null() }
    }

    /// Resets the driver runtime to zero threads. This may only be called when there are no
    /// existing dispatchers.
    ///
    /// # Panics
    ///
    /// This method should not be called from a thread managed by the driver runtime,
    /// such as from tasks or ChannelRead callbacks.
    pub fn reset(&self) {
        assert!(
            Self::current_thread_managed_by_driver_runtime() == false,
            "reset must be called from a thread not managed by the driver runtime"
        );
        // SAFETY: calling fdf_env_reset, which does not have any soundness
        // concerns for rust code. It may be called multiple times without any problems.
        unsafe { fdf_env_reset() };
    }

    /// Destroys all dispatchers in the process and blocks the current thread
    /// until each runtime dispatcher in the process is observed to have been destroyed.
    ///
    /// This should only be used called after all drivers have been shutdown.
    ///
    /// # Panics
    ///
    /// This method should not be called from a thread managed by the driver runtime,
    /// such as from tasks or ChannelRead callbacks.
    pub fn destroy_all_dispatchers(&self) {
        assert!(Self::current_thread_managed_by_driver_runtime() == false,
            "destroy_all_dispatchers must be called from a thread not managed by the driver runtime");
        unsafe { fdf_env_destroy_all_dispatchers() };
    }

    /// Returns whether the dispatcher has any queued tasks.
    pub fn dispatcher_has_queued_tasks(&self, dispatcher: DispatcherRef<'_>) -> bool {
        unsafe { fdf_env_dispatcher_has_queued_tasks(dispatcher.inner().as_ptr()) }
    }

    /// Returns the current maximum number of threads which will be spawned for thread pool associated
    /// with the given scheduler role.
    ///
    /// |scheduler_role| is the name of the role which is passed when creating dispatchers.
    pub fn get_thread_limit(&self, scheduler_role: &str) -> u32 {
        let scheduler_role_ptr = scheduler_role.as_ptr() as *mut ffi::c_char;
        let scheduler_role_len = scheduler_role.len();
        unsafe { fdf_env_get_thread_limit(scheduler_role_ptr, scheduler_role_len) }
    }

    /// Sets the number of threads which will be spawned for thread pool associated with the given
    /// scheduler role. It cannot shrink the limit less to a value lower than the current number of
    /// threads in the thread pool.
    ///
    /// |scheduler_role| is the name of the role which is passed when creating dispatchers.
    /// |max_threads| is the number of threads to use as new limit.
    pub fn set_thread_limit(&self, scheduler_role: &str, max_threads: u32) -> Result<(), Status> {
        let scheduler_role_ptr = scheduler_role.as_ptr() as *mut ffi::c_char;
        let scheduler_role_len = scheduler_role.len();
        Status::ok(unsafe {
            fdf_env_set_thread_limit(scheduler_role_ptr, scheduler_role_len, max_threads)
        })
    }

    /// Gets the driver currently running on the thread identified by |thread_koid|, if the thread
    /// is running on this driver host with a driver.
    pub fn get_driver_on_thread_koid(&self, thread_koid: zx::Koid) -> Option<UnownedDriver> {
        let mut driver = std::ptr::null();
        unsafe {
            Status::ok(fdf_env_get_driver_on_tid(thread_koid.raw_koid(), &mut driver)).ok()?;
        }
        if driver.is_null() {
            None
        } else {
            Some(UnownedDriver { inner: driver })
        }
    }
}
