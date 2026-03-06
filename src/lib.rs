#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![doc = include_str!("../README.md")]

#[cfg(feature = "test-hooks")]
pub mod hooks;

use std::{
    fmt::Debug,
    mem::{forget, take},
    ops::{Deref, DerefMut},
    panic::{self, AssertUnwindSafe},
};

use parking_lot::{
    Condvar, MappedRwLockReadGuard, MappedRwLockWriteGuard, Mutex, RwLock, RwLockReadGuard,
    RwLockWriteGuard,
};

#[derive(Default)]
struct Inner {
    dropping: bool,
    locking: u64,
    /// Number of live [`RwLockNotifyReadGuard`] instances.
    readers: u64,
    callbacks: Vec<Box<dyn FnOnce() + Send>>,
}

impl Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("dropping", &self.dropping)
            .field("locking", &self.locking)
            .field("readers", &self.readers)
            .field("callbacks", &"{callbacks}")
            .finish()
    }
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
            Some(inner) => Debug::fmt(&inner, f),
            None => f
                .debug_struct("LockState")
                .field("inner", &"<locked>")
                .finish(),
        }
    }
}

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
    state: LockState,
}

impl<T> RwLockNotify<T> {
    /// Creates a new `RwLockNotify` wrapping `value`.
    #[inline]
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::from_lock(RwLock::new(value))
    }

    /// Creates a new `RwLockNotify` from an existing [`RwLock`].
    #[inline]
    #[must_use]
    pub fn from_lock(lock: RwLock<T>) -> Self {
        Self {
            lock,
            state: LockState::default(),
        }
    }

    /// Consumes the lock and returns the inner value.
    ///
    /// Any callbacks pending in the queue are dropped without being called.
    #[inline]
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
    #[must_use]
    pub fn read(&self) -> RwLockNotifyReadGuard<'_, T> {
        let guard = self.lock.read();
        self.state.inner.lock().readers += 1;
        RwLockNotifyReadGuard {
            guard: Some(guard),
            state: &self.state,
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
            state: &self.state,
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
            state: &self.state,
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
            state: &self.state,
        })
    }

    /// Attempts to acquire exclusive write access without blocking.
    ///
    /// - **Success** — returns `Some(guard)` and discards `callback`.
    /// - **Failure** — queues `callback` and returns `None`. The callback will be
    ///   called, without holding any lock, after the next write guard is dropped.
    ///
    /// Callbacks are called in FIFO registration order.
    #[inline]
    #[must_use]
    pub fn try_write_or<'a, Callback>(
        &'a self,
        callback: Callback,
    ) -> Option<RwLockNotifyWriteGuard<'a, T>>
    where
        Callback: FnOnce() + Send + 'static,
    {
        self.try_write_or_else(|| callback)
    }

    /// Attempts to acquire exclusive write access without blocking, lazily constructing
    /// the callback only if the lock is unavailable.
    ///
    /// - **Success** — returns `Some(guard)` and never calls `callback`.
    /// - **Failure** — calls `callback()` to produce the callback, queues it, and returns
    ///   `None`. The callback will be called, without holding any lock, after the next
    ///   write guard is dropped.
    ///
    /// Prefer this over [`try_write_or`] when constructing the callback is expensive
    /// or has side effects that should only occur on failure.
    ///
    /// Callbacks are called in FIFO registration order.
    ///
    /// [`try_write_or`]: Self::try_write_or
    #[must_use]
    pub fn try_write_or_else<'a, Callback>(
        &'a self,
        callback: impl FnOnce() -> Callback,
    ) -> Option<RwLockNotifyWriteGuard<'a, T>>
    where
        Callback: FnOnce() + Send + 'static,
    {
        // Atomically wait until not dropping, then increment locking.
        // Both steps happen under the same mutex, which eliminates the TOCTOU
        // that required SeqCst atomic ordering in the spin-based version.
        let mut inner = self.state.inner.lock();

        while inner.dropping {
            #[cfg(feature = "test-hooks")]
            hooks::run(hooks::HookPoint::TryWriteOrWhileDropping);

            self.state.not_dropping.wait(&mut inner);
        }
        inner.locking += 1;
        drop(inner);

        #[cfg(feature = "test-hooks")]
        hooks::run(hooks::HookPoint::TryWriteOrBeforeAcquire);

        if let Some(guard) = self.lock.try_write() {
            let mut inner = self.state.inner.lock();
            inner.locking -= 1;
            if inner.locking == 0 {
                self.state.locking_zero.notify_one();
            }
            Some(RwLockNotifyWriteGuard {
                guard: Some(guard),
                state: &self.state,
            })
        } else {
            let mut inner = self.state.inner.lock();
            inner.callbacks.push(Box::new(callback()));
            inner.locking -= 1;
            if inner.locking == 0 {
                self.state.locking_zero.notify_one();
            }
            None
        }
    }
}

impl<T> From<T> for RwLockNotify<T> {
    #[inline]
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> From<RwLock<T>> for RwLockNotify<T> {
    #[inline]
    fn from(lock: RwLock<T>) -> Self {
        Self::from_lock(lock)
    }
}

impl<T> From<RwLockNotify<T>> for RwLock<T> {
    #[inline]
    fn from(lock: RwLockNotify<T>) -> Self {
        lock.lock
    }
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
    state: &'a LockState,
}

impl<'a, T> RwLockNotifyReadGuard<'a, T> {
    /// Transforms this guard into a [`MappedRwLockNotifyReadGuard`] that dereferences
    /// to a subfield of the protected value.
    pub fn map<U, F>(mut self, f: F) -> MappedRwLockNotifyReadGuard<'a, U>
    where
        F: FnOnce(&T) -> &U,
    {
        let guard = self.guard.take().unwrap();
        let state = self.state;
        forget(self);
        let map_guard = RwLockReadGuard::map(guard, f);
        MappedRwLockNotifyReadGuard {
            guard: Some(map_guard),
            state,
        }
    }

    /// Attempts to transform this guard into a [`MappedRwLockNotifyReadGuard`].
    ///
    /// Returns `Err(self)` if `f` returns `None`, giving the original guard back.
    pub fn try_map<U, F>(mut self, f: F) -> Result<MappedRwLockNotifyReadGuard<'a, U>, Self>
    where
        F: FnOnce(&T) -> Option<&U>,
    {
        let guard = self.guard.take().unwrap();
        let state = self.state;
        forget(self);
        match RwLockReadGuard::try_map(guard, f) {
            Ok(map_guard) => Ok(MappedRwLockNotifyReadGuard {
                guard: Some(map_guard),
                state,
            }),
            Err(guard) => Err(Self {
                guard: Some(guard),
                state,
            }),
        }
    }

    /// Attempts to transform this guard into a [`MappedRwLockNotifyReadGuard`].
    ///
    /// Returns `Err((self, error))` if `f` returns `Err`, giving the original guard
    /// and the error back.
    pub fn try_map_or_err<U, F, E>(
        mut self,
        f: F,
    ) -> Result<MappedRwLockNotifyReadGuard<'a, U>, (Self, E)>
    where
        F: FnOnce(&T) -> Result<&U, E>,
    {
        let guard = self.guard.take().unwrap();
        let state = self.state;
        forget(self);
        match RwLockReadGuard::try_map_or_err(guard, f) {
            Ok(map_guard) => Ok(MappedRwLockNotifyReadGuard {
                guard: Some(map_guard),
                state,
            }),
            Err((guard, err)) => Err((
                Self {
                    guard: Some(guard),
                    state,
                },
                err,
            )),
        }
    }
}

impl<'a, T> Deref for RwLockNotifyReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> Drop for RwLockNotifyReadGuard<'a, T> {
    fn drop(&mut self) {
        drop_read_guard(&mut self.guard, self.state)
    }
}

/// RAII read guard produced by [`RwLockNotifyReadGuard::map`] and related methods.
///
/// Provides shared read access to a subfield of the protected value via [`Deref`].
/// When dropped, releases the read lock and flushes pending callbacks just like
/// [`RwLockNotifyReadGuard`].
#[derive(Debug)]
pub struct MappedRwLockNotifyReadGuard<'a, T> {
    guard: Option<MappedRwLockReadGuard<'a, T>>,
    state: &'a LockState,
}

impl<'a, T> Deref for MappedRwLockNotifyReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> Drop for MappedRwLockNotifyReadGuard<'a, T> {
    fn drop(&mut self) {
        drop_read_guard(&mut self.guard, self.state)
    }
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
    state: &'a LockState,
}

impl<'a, T> RwLockNotifyWriteGuard<'a, T> {
    /// Transforms this guard into a [`MappedRwLockNotifyWriteGuard`] that dereferences
    /// to a subfield of the protected value.
    pub fn map<U, F>(mut self, f: F) -> MappedRwLockNotifyWriteGuard<'a, U>
    where
        F: FnOnce(&mut T) -> &mut U,
    {
        let guard = self.guard.take().unwrap();
        let state = self.state;
        forget(self);
        let map_guard = RwLockWriteGuard::map(guard, f);
        MappedRwLockNotifyWriteGuard {
            guard: Some(map_guard),
            state,
        }
    }

    /// Attempts to transform this guard into a [`MappedRwLockNotifyWriteGuard`].
    ///
    /// Returns `Err(self)` if `f` returns `None`, giving the original guard back.
    pub fn try_map<U, F>(mut self, f: F) -> Result<MappedRwLockNotifyWriteGuard<'a, U>, Self>
    where
        F: FnOnce(&mut T) -> Option<&mut U>,
    {
        let guard = self.guard.take().unwrap();
        let state = self.state;
        forget(self);
        match RwLockWriteGuard::try_map(guard, f) {
            Ok(map_guard) => Ok(MappedRwLockNotifyWriteGuard {
                guard: Some(map_guard),
                state,
            }),
            Err(guard) => Err(Self {
                guard: Some(guard),
                state,
            }),
        }
    }

    /// Attempts to transform this guard into a [`MappedRwLockNotifyWriteGuard`].
    ///
    /// Returns `Err((self, error))` if `f` returns `Err`, giving the original guard
    /// and the error back.
    pub fn try_map_err<U, F, E>(
        mut self,
        f: F,
    ) -> Result<MappedRwLockNotifyWriteGuard<'a, U>, (Self, E)>
    where
        F: FnOnce(&mut T) -> Result<&mut U, E>,
    {
        let guard = self.guard.take().unwrap();
        let state = self.state;
        forget(self);
        match RwLockWriteGuard::try_map_or_err(guard, f) {
            Ok(map_guard) => Ok(MappedRwLockNotifyWriteGuard {
                guard: Some(map_guard),
                state,
            }),
            Err((guard, err)) => Err((
                Self {
                    guard: Some(guard),
                    state,
                },
                err,
            )),
        }
    }
}

impl<'a, T> Deref for RwLockNotifyWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> DerefMut for RwLockNotifyWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        DerefMut::deref_mut(self.guard.as_mut().unwrap())
    }
}

impl<'a, T> Drop for RwLockNotifyWriteGuard<'a, T> {
    fn drop(&mut self) {
        drop_write_guard(&mut self.guard, self.state)
    }
}

/// RAII write guard produced by [`RwLockNotifyWriteGuard::map`] and related methods.
///
/// Provides exclusive write access to a subfield of the protected value via [`Deref`]
/// and [`DerefMut`]. When dropped, releases the lock and flushes pending callbacks
/// just like [`RwLockNotifyWriteGuard`].
#[derive(Debug)]
pub struct MappedRwLockNotifyWriteGuard<'a, T> {
    guard: Option<MappedRwLockWriteGuard<'a, T>>,
    state: &'a LockState,
}

impl<'a, T> Deref for MappedRwLockNotifyWriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> DerefMut for MappedRwLockNotifyWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        DerefMut::deref_mut(self.guard.as_mut().unwrap())
    }
}

impl<'a, T> Drop for MappedRwLockNotifyWriteGuard<'a, T> {
    fn drop(&mut self) {
        drop_write_guard(&mut self.guard, self.state)
    }
}

fn drop_read_guard<G>(guard: &mut Option<G>, state: &LockState) {
    drop(guard.take());

    #[cfg(feature = "test-hooks")]
    hooks::run(hooks::HookPoint::ReadGuardAfterRelease);

    let callbacks = {
        let mut inner = state.inner.lock();
        inner.readers -= 1;
        // Only the last reader drains; also skip if a concurrent drain is
        // already running (`dropping = true`) — it would create a deadlock
        // because both sides would sleep on `locking_zero` but
        // `notify_one` only wakes a single waiter.
        if inner.readers > 0 || inner.callbacks.is_empty() || inner.dropping {
            return;
        }
        inner.dropping = true;

        #[cfg(feature = "test-hooks")]
        hooks::run(hooks::HookPoint::ReadGuardAfterSettingDropping);

        while inner.locking != 0 {
            state.locking_zero.wait(&mut inner);
        }
        take(&mut inner.callbacks)
    };

    // The read lock is already released; pass `()` so drain_and_run has
    // nothing to drop before resetting `dropping` and running callbacks.
    drain_and_run(state, callbacks);
}

fn drop_write_guard<G>(guard: &mut Option<G>, state: &LockState) {
    #[cfg(feature = "test-hooks")]
    hooks::run(hooks::HookPoint::WriteGuardBeforeDrop);

    let callbacks = {
        let mut inner = state.inner.lock();
        inner.dropping = true;

        #[cfg(feature = "test-hooks")]
        hooks::run(hooks::HookPoint::WriteGuardAfterSettingDropping);

        // Sleep until all in-flight try_write_or calls have either
        // registered their callback or obtained the lock.
        while inner.locking != 0 {
            state.locking_zero.wait(&mut inner);
        }
        take(&mut inner.callbacks)
        // Mutex released here — callbacks execute without holding any lock.
    };

    drop(guard.take().unwrap());

    drain_and_run(state, callbacks);
}

/// Resets `dropping` to `false`, notifies waiters, then runs all collected
/// callbacks, re-raising the first panic.
///
/// Callers must have already set `dropping = true` and drained the callback queue.
fn drain_and_run(state: &LockState, callbacks: Vec<Box<dyn FnOnce() + Send>>) {
    #[cfg(feature = "test-hooks")]
    hooks::run(hooks::HookPoint::DrainAfterWriteLockRelease);

    {
        let mut inner = state.inner.lock();
        inner.dropping = false;
        state.not_dropping.notify_all();
    }

    #[cfg(feature = "test-hooks")]
    hooks::run(hooks::HookPoint::DrainBeforeCallbacks);

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
