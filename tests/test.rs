use std::{
    hint::spin_loop,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed},
    },
    thread,
};

use lock_callback::RwLockCallback;
use parking_lot::{Mutex, RwLock};

#[test]
fn test_callback() {
    let lock = RwLock::new(12u64);
    let lock_callback = Arc::new(RwLockCallback::new(lock));

    let step = Arc::new(AtomicU64::new(0));
    let callback_allowed = Arc::new(AtomicBool::new(false));

    thread::scope(|s| {
        let step_1 = step.clone();
        let step_2 = step.clone();

        let lock_callback_1 = lock_callback.clone();
        let lock_callback_2 = lock_callback.clone();

        let callback_allowed_1 = callback_allowed.clone();
        let callback_allowed_2 = callback_allowed.clone();

        s.spawn(move || {
            let guard = lock_callback_1.try_write(|| {}).unwrap();
            step_1.fetch_add(1, Relaxed);

            while step_1.load(Relaxed) != 2 {
                spin_loop();
            }

            callback_allowed_1.store(true, Relaxed);

            drop(guard);

            callback_allowed_1.store(false, Relaxed);
        });

        s.spawn(move || {
            while step_2.load(Relaxed) != 1 {
                spin_loop();
            }

            let r = lock_callback_2.try_write(move || {
                assert!(callback_allowed_2.load(Relaxed));
            });
            assert!(r.is_none());
            step_2.fetch_add(1, Relaxed);
        });
    });
}

#[test]
fn test_multiple_callbacks() {
    let lock = Arc::new(RwLockCallback::new(RwLock::new(0u64)));
    let order = Arc::new(Mutex::new(Vec::new()));

    let guard = lock.try_write(|| {}).unwrap();

    for i in 0..5u64 {
        let order = order.clone();
        lock.try_write(move || order.lock().push(i));
    }

    drop(guard);
    assert_eq!(*order.lock(), vec![0, 1, 2, 3, 4]);
}

#[test]
fn test_callback_can_call_try_write() {
    // Verifies that callbacks execute without holding any internal lock,
    // so re-entering try_write from a callback does not deadlock.
    let lock = Arc::new(RwLockCallback::new(RwLock::new(0u64)));
    let lock2 = lock.clone();

    let guard = lock.try_write(|| {}).unwrap();
    lock.try_write(move || {
        // Lock is free here — this must not deadlock.
        let _ = lock2.try_write(|| {});
    });
    drop(guard);
}

#[test]
fn test_read() {
    let lock = RwLockCallback::new(42u64);
    assert_eq!(*lock.read(), 42);
}

#[test]
fn test_concurrent_reads() {
    // Two read guards can be held simultaneously.
    let lock = Arc::new(RwLockCallback::new(42u64));
    let r1 = lock.read();
    let r2 = lock.read();
    assert_eq!(*r1, 42);
    assert_eq!(*r2, 42);
}

#[test]
fn test_write_triggers_callbacks() {
    // write() (blocking) should run registered callbacks on drop, just like
    // a guard obtained via try_write().
    let lock = Arc::new(RwLockCallback::new(RwLock::new(0u64)));

    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let guard = lock.write();
    lock.try_write(move || called2.store(true, Relaxed));
    drop(guard);

    assert!(called.load(Relaxed));
}
