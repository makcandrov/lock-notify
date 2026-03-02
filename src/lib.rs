#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![doc = include_str!("../README.md")]

use std::{
    fmt::Debug,
    mem::take,
    ops::{Deref, DerefMut},
    panic::{self, AssertUnwindSafe},
    sync::Arc,
};

use parking_lot::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// An [`RwLock`] wrapper that fires registered callbacks when a write guard is released.
///
/// When [`try_write_or`] cannot acquire the lock, the provided callback is queued.
/// All queued callbacks are called in FIFO order, without holding any lock, when the
/// next write guard is dropped.
///
/// [`try_write_or`]: RwLockNotify::try_write_or
#[derive(Debug)]
pub struct RwLockNotify<T> {
    lock: RwLock<T>,
    state: Arc<LockState>,
}

/// RAII read guard for [`RwLockNotify`].
///
/// Provides shared read access to the protected value via [`Deref`].
/// When dropped, releases the read lock and, if this was the last active
/// read guard, immediately flushes any pending callbacks that were registered
/// via [`try_write_or`].
///
/// Obtained via [`RwLockNotify::read`] or [`RwLockNotify::try_read`].
///
/// [`try_write_or`]: RwLockNotify::try_write_or
#[derive(Debug)]
pub struct RwLockNotifyReadGuard<'a, T> {
    guard: Option<RwLockReadGuard<'a, T>>,
    state: Arc<LockState>,
}

/// RAII write guard for [`RwLockNotify`].
///
/// Provides exclusive write access to the protected value via [`Deref`] and [`DerefMut`].
/// When dropped, releases the lock and calls all callbacks that were registered via
/// [`try_write_or`] while this guard was held.
///
/// Obtained via [`RwLockNotify::write`], [`RwLockNotify::try_write`], or
/// [`RwLockNotify::try_write_or`].
///
/// [`try_write_or`]: RwLockNotify::try_write_or
#[derive(Debug)]
pub struct RwLockNotifyWriteGuard<'a, T> {
    guard: Option<RwLockWriteGuard<'a, T>>,
    state: Arc<LockState>,
}

#[derive(Default)]
struct Inner {
    dropping: bool,
    locking: u64,
    /// Number of live [`RwLockNotifyReadGuard`] instances.
    readers: u64,
    callbacks: Vec<Box<dyn FnOnce() + Send>>,
}

#[derive(Default)]
struct LockState {
    inner: Mutex<Inner>,
    /// Notified when `locking` reaches zero (dropper is waiting on this).
    locking_zero: Condvar,
    /// Notified when `dropping` becomes false (try_write callers wait on this).
    not_dropping: Condvar,
}

impl Debug for LockState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.try_lock() {
            Some(inner) => f
                .debug_struct("LockState")
                .field("dropping", &inner.dropping)
                .field("locking", &inner.locking)
                .field("callbacks", &"{callbacks}")
                .finish(),
            None => f
                .debug_struct("LockState")
                .field("inner", &"<locked>")
                .finish(),
        }
    }
}

impl<T> RwLockNotify<T> {
    /// Creates a new `RwLockNotify` wrapping `value`.
    pub fn new(value: T) -> Self {
        Self::from_inner(RwLock::new(value))
    }

    /// Creates a new `RwLockNotify` from an existing [`RwLock`].
    pub fn from_inner(lock: RwLock<T>) -> Self {
        Self {
            lock,
            state: Arc::new(LockState::default()),
        }
    }

    /// Consumes the lock and returns the inner value.
    ///
    /// Any callbacks pending in the queue are dropped without being called.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.lock.into_inner()
    }

    /// Locks for shared read access, blocking until it can be acquired.
    ///
    /// When the returned guard is dropped, any pending callbacks registered via
    /// [`try_write_or`] are flushed if this was the last reader holding the lock.
    ///
    /// [`try_write_or`]: RwLockNotify::try_write_or
    pub fn read(&self) -> RwLockNotifyReadGuard<'_, T> {
        let guard = self.lock.read();
        self.state.inner.lock().readers += 1;
        RwLockNotifyReadGuard {
            guard: Some(guard),
            state: self.state.clone(),
        }
    }

    /// Attempts to acquire shared read access without blocking.
    ///
    /// Returns `None` if a write lock is currently held.
    #[must_use]
    pub fn try_read(&self) -> Option<RwLockNotifyReadGuard<'_, T>> {
        let guard = self.lock.try_read()?;
        self.state.inner.lock().readers += 1;
        Some(RwLockNotifyReadGuard {
            guard: Some(guard),
            state: self.state.clone(),
        })
    }

    /// Locks for exclusive write access, blocking until it can be acquired.
    ///
    /// Callbacks registered via [`try_write_or`] while this guard is held will be
    /// called when the guard is dropped.
    ///
    /// [`try_write_or`]: RwLockNotify::try_write_or
    #[must_use]
    pub fn write(&self) -> RwLockNotifyWriteGuard<'_, T> {
        RwLockNotifyWriteGuard {
            guard: Some(self.lock.write()),
            state: self.state.clone(),
        }
    }

    /// Attempts to acquire exclusive write access without blocking.
    ///
    /// Returns `None` if the lock is currently held. No callback is registered on
    /// failure; use [`try_write_or`] to register one.
    ///
    /// [`try_write_or`]: RwLockNotify::try_write_or
    #[must_use]
    pub fn try_write(&self) -> Option<RwLockNotifyWriteGuard<'_, T>> {
        self.lock.try_write().map(|guard| RwLockNotifyWriteGuard {
            guard: Some(guard),
            state: self.state.clone(),
        })
    }

    /// Attempts to acquire exclusive write access without blocking.
    ///
    /// - **Success** — returns `Some(guard)` and discards `callback`.
    /// - **Failure** — queues `callback` and returns `None`. The callback will be
    ///   called, without holding any lock, after the next write guard is dropped.
    ///
    /// Callbacks are called in FIFO registration order.
    #[must_use]
    pub fn try_write_or<'a>(
        &'a self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Option<RwLockNotifyWriteGuard<'a, T>> {
        // Atomically wait until not dropping, then increment locking.
        // Both steps happen under the same mutex, which eliminates the TOCTOU
        // that required SeqCst atomic ordering in the spin-based version.
        let mut inner = self.state.inner.lock();
        while inner.dropping {
            self.state.not_dropping.wait(&mut inner);
        }
        inner.locking += 1;
        drop(inner);

        if let Some(guard) = self.lock.try_write() {
            let mut inner = self.state.inner.lock();
            inner.locking -= 1;
            if inner.locking == 0 {
                self.state.locking_zero.notify_one();
            }
            Some(RwLockNotifyWriteGuard {
                guard: Some(guard),
                state: self.state.clone(),
            })
        } else {
            let mut inner = self.state.inner.lock();
            inner.callbacks.push(Box::new(callback));
            inner.locking -= 1;
            if inner.locking == 0 {
                self.state.locking_zero.notify_one();
            }
            None
        }
    }
}

impl<T> From<T> for RwLockNotify<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> From<RwLock<T>> for RwLockNotify<T> {
    fn from(lock: RwLock<T>) -> Self {
        Self::from_inner(lock)
    }
}

/// Drops `write_guard` (releasing the exclusive lock), resets `dropping` to `false`,
/// notifies waiters, then runs all collected callbacks, re-raising the first panic.
///
/// Callers must have already set `dropping = true` and drained the callback queue.
fn drain_and_run<W>(
    write_guard: W,
    state: &Arc<LockState>,
    callbacks: Vec<Box<dyn FnOnce() + Send>>,
) {
    drop(write_guard);

    {
        let mut inner = state.inner.lock();
        inner.dropping = false;
        state.not_dropping.notify_all();
    }

    // Run every callback regardless of panics, then re-raise the first
    // panic (if any) after all callbacks have had a chance to execute.
    // `catch_unwind` is called unconditionally before `or` so that every
    // callback runs even if an earlier one panicked (`or_else` would
    // short-circuit and skip the remaining callbacks).
    let first_panic = callbacks.into_iter().fold(None, |first, callback| {
        let result = panic::catch_unwind(AssertUnwindSafe(callback)).err();
        first.or(result)
    });

    if let Some(payload) = first_panic {
        panic::resume_unwind(payload);
    }
}

impl<'a, T> Drop for RwLockNotifyReadGuard<'a, T> {
    fn drop(&mut self) {
        drop(self.guard.take());

        let callbacks = {
            let mut inner = self.state.inner.lock();
            inner.readers -= 1;
            // Only the last reader drains; also skip if a concurrent drain is
            // already running (`dropping = true`) — it would create a deadlock
            // because both sides would sleep on `locking_zero` but
            // `notify_one` only wakes a single waiter.
            if inner.readers > 0 || inner.callbacks.is_empty() || inner.dropping {
                return;
            }
            inner.dropping = true;
            while inner.locking != 0 {
                self.state.locking_zero.wait(&mut inner);
            }
            take(&mut inner.callbacks)
        };

        // The read lock is already released; pass `()` so drain_and_run has
        // nothing to drop before resetting `dropping` and running callbacks.
        drain_and_run((), &self.state, callbacks);
    }
}

impl<'a, T> Drop for RwLockNotifyWriteGuard<'a, T> {
    fn drop(&mut self) {
        let callbacks = {
            let mut inner = self.state.inner.lock();
            inner.dropping = true;
            // Sleep until all in-flight try_write_or calls have either
            // registered their callback or obtained the lock.
            while inner.locking != 0 {
                self.state.locking_zero.wait(&mut inner);
            }
            take(&mut inner.callbacks)
            // Mutex released here — callbacks execute without holding any lock.
        };

        drain_and_run(self.guard.take().unwrap(), &self.state, callbacks);
    }
}

impl<'a, T> Deref for RwLockNotifyReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> Deref for RwLockNotifyWriteGuard<'a, T> {
    type Target = <RwLockWriteGuard<'a, T> as Deref>::Target;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> DerefMut for RwLockNotifyWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        DerefMut::deref_mut(self.guard.as_mut().unwrap())
    }
}
