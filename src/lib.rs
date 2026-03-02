use std::{
     fmt::Debug, hint::spin_loop, mem::take, sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::{SeqCst, Release, Relaxed}},
    }
};

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};

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
struct LockState {
    locking: AtomicU64,
    dropping: AtomicBool,
    callbacks: RwLock<Vec<Box<dyn FnOnce() + Send + Sync>>>,
}

impl Debug for LockState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LockState")
            .field("locking", &self.locking)
            .field("dropping", &self.dropping)
            .field("callbacks", &"{callbacks}")
            .finish()
    }
}

impl<T> RwLockCallback<T> {
    pub fn new(lock: RwLock<T>) -> Self {
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
        callback: impl FnOnce() + Send + Sync + 'static,
    ) -> Option<RwLockCallbackWriteGuard<'a, T>> {
        loop {
            self.state.locking.fetch_add(1, SeqCst);

            if !self.state.dropping.load(SeqCst) {
                break;
            }

            self.state.locking.fetch_sub(1, SeqCst);
            while self.state.dropping.load(Relaxed) {
                spin_loop();
            }
        }

        if let Some(guard) = self.lock.try_write() {
            self.state.locking.fetch_sub(1, Release);

            Some(RwLockCallbackWriteGuard {
                guard: Some(guard),
                state: self.state.clone(),
            })
        } else {
            self.state.callbacks.write().push(Box::new(callback));
            self.state.locking.fetch_sub(1, Release);
            None
        }
    }
}

impl<'a, T> Drop for RwLockCallbackWriteGuard<'a, T> {
    fn drop(&mut self) {
        self.state.dropping.store(true, SeqCst);
        let guard = self.guard.take().unwrap();

        let callbacks = loop {
            let mut callbacks = self.state.callbacks.write();

            if self.state.locking.load(SeqCst) == 0 {
                break take(&mut *callbacks);
            }

            spin_loop();
        };

        drop(guard);
        self.state.dropping.store(false, Relaxed);

        for callback in callbacks {
            callback()
        }
    }
}
