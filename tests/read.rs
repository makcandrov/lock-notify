use std::{
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    },
};

use lockbell::RwLockBell;
use parking_lot::Mutex;

#[test]
fn test_callback_fires_on_single_read_guard_drop() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    assert!(
        !called.load(Relaxed),
        "callback must not fire while read guard is held"
    );
    drop(r);
    assert!(
        called.load(Relaxed),
        "callback must fire when the read guard drops"
    );
}

#[test]
fn test_callback_fires_on_try_read_guard_drop() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r = lock.try_read().expect("lock is free");
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    assert!(!called.load(Relaxed));
    drop(r);
    assert!(called.load(Relaxed));
}

#[test]
fn test_callback_fires_on_last_read_guard_drop() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r1 = lock.read();
    let r2 = lock.read();
    let r3 = lock.read();

    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    drop(r1);
    assert!(!called.load(Relaxed), "should not fire after first drop");
    drop(r2);
    assert!(!called.load(Relaxed), "should not fire after second drop");
    drop(r3);
    assert!(called.load(Relaxed), "must fire after last drop");
}

#[test]
fn test_multiple_callbacks_fired_by_last_read_guard() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let order = Arc::new(Mutex::new(Vec::<u64>::new()));

    let r = lock.read();

    for i in 0..5u64 {
        let order = order.clone();
        assert!(lock.try_write_or(move || order.lock().push(i)).is_none());
    }

    drop(r);
    assert_eq!(*order.lock(), vec![0, 1, 2, 3, 4]);
}

#[test]
fn test_callback_no_lock_held_read_path() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let lock2 = lock.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || {
            let mut w = lock2.write();
            *w = 99;
        })
        .is_none()
    );

    drop(r);
    assert_eq!(*lock.read(), 99);
}

#[test]
fn test_callback_from_read_guard_drain_can_re_register() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let second_called = Arc::new(AtomicBool::new(false));
    let second_called2 = second_called.clone();
    let lock2 = lock.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || {
            let _guard = lock2
                .try_write_or(|| {})
                .expect("lock is free during callback, try_write_or must succeed");
            second_called2.store(true, Relaxed);
        })
        .is_none()
    );

    drop(r);
    assert!(second_called.load(Relaxed));
}

#[test]
fn test_callback_panic_read_guard_path() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let second_called = Arc::new(AtomicBool::new(false));
    let second_called2 = second_called.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(|| panic!("intentional panic from read guard callback"))
            .is_none()
    );
    assert!(
        lock.try_write_or(move || second_called2.store(true, Relaxed))
            .is_none()
    );

    let result = panic::catch_unwind(AssertUnwindSafe(|| drop(r)));
    assert!(result.is_err(), "panic from callback must propagate");
    assert!(
        second_called.load(Relaxed),
        "second callback must still run"
    );
}

#[test]
fn test_callback_fires_immediately_not_deferred() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    drop(r);
    assert!(
        called.load(Relaxed),
        "callback must fire synchronously in the drop of the last read guard"
    );
}
