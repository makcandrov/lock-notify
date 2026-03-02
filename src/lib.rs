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
pub struct RwLockNotify<T> {
    lock: RwLock<T>,
    state: Arc<LockState>,
}

#[derive(Debug)]
pub struct RwLockNotifyWriteGuard<'a, T> {
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

impl<T> RwLockNotify<T> {
    pub fn new(value: T) -> Self {
        Self::from_inner(RwLock::new(value))
    }

    pub fn from_inner(lock: RwLock<T>) -> Self {
        Self {
            lock,
            state: Arc::new(LockState::default()),
        }
    }

    #[must_use]
    pub fn into_inner(self) -> T {
        self.lock.into_inner()
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.lock.read()
    }

    #[must_use]
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        self.lock.try_read()
    }

    #[must_use]
    pub fn write(&self) -> RwLockNotifyWriteGuard<'_, T> {
        RwLockNotifyWriteGuard {
            guard: Some(self.lock.write()),
            state: self.state.clone(),
        }
    }

    #[must_use]
    pub fn try_write(&self) -> Option<RwLockNotifyWriteGuard<'_, T>> {
        self.lock.try_write().map(|guard| RwLockNotifyWriteGuard {
            guard: Some(guard),
            state: self.state.clone(),
        })
    }

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
