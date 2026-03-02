#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![doc = include_str!("../README.md")]

use std::{
    fmt::Debug,
    mem::take,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use parking_lot::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Debug)]
pub struct RwLockCallback<T> {
    lock: RwLock<T>,
    state: Arc<LockState>,
}

#[derive(Debug)]
pub struct RwLockCallbackWriteGuard<'a, T> {
    guard: Option<RwLockWriteGuard<'a, T>>,
    state: Arc<LockState>,
}

#[derive(Default)]
struct Inner {
    dropping: bool,
    locking: u64,
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

impl<T> RwLockCallback<T> {
    pub fn new_arc(value: T) -> Arc<Self> {
        Arc::new(Self::new(value))
    }

    pub fn new(value: T) -> Self {
        Self::from_lock(RwLock::new(value))
    }

    pub fn from_lock(lock: RwLock<T>) -> Self {
        Self {
            lock,
            state: Arc::new(LockState::default()),
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.lock.read()
    }

    pub fn write(&self) -> RwLockCallbackWriteGuard<'_, T> {
        RwLockCallbackWriteGuard {
            guard: Some(self.lock.write()),
            state: self.state.clone(),
        }
    }

    pub fn try_write<'a>(
        &'a self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Option<RwLockCallbackWriteGuard<'a, T>> {
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
            Some(RwLockCallbackWriteGuard {
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

impl<T> From<T> for RwLockCallback<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> From<RwLock<T>> for RwLockCallback<T> {
    fn from(lock: RwLock<T>) -> Self {
        Self::from_lock(lock)
    }
}

impl<'a, T> Drop for RwLockCallbackWriteGuard<'a, T> {
    fn drop(&mut self) {
        let callbacks = {
            let mut inner = self.state.inner.lock();
            inner.dropping = true;
            // Sleep until all in-flight try_write calls have either registered
            // their callback or obtained the lock.
            while inner.locking != 0 {
                self.state.locking_zero.wait(&mut inner);
            }
            take(&mut inner.callbacks)
            // Mutex released here — callbacks execute without holding any lock.
        };

        drop(self.guard.take().unwrap());

        {
            let mut inner = self.state.inner.lock();
            inner.dropping = false;
            self.state.not_dropping.notify_all();
        }

        for callback in callbacks {
            callback()
        }
    }
}

impl<'a, T> Deref for RwLockCallbackWriteGuard<'a, T> {
    type Target = <RwLockWriteGuard<'a, T> as Deref>::Target;

    fn deref(&self) -> &Self::Target {
        Deref::deref(self.guard.as_ref().unwrap())
    }
}

impl<'a, T> DerefMut for RwLockCallbackWriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        DerefMut::deref_mut(self.guard.as_mut().unwrap())
    }
}
