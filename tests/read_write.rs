use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering::Relaxed},
};

use lockbell::RwLockBell;

#[test]
fn test_dropping_flag_prevents_double_drain() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let w = lock.write();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );
    drop(w);

    assert!(called.load(Relaxed));

    let r = lock.read();
    drop(r);
}

#[test]
fn test_write_guard_drop_then_read_guard_drop_sequence() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    let first2 = first.clone();
    let second2 = second.clone();

    {
        let w = lock.write();
        assert_eq!(*w, 0);
        assert!(
            lock.try_write_or(move || first2.store(true, Relaxed))
                .is_none()
        );
        drop(w);
    }
    assert!(first.load(Relaxed));

    let r = lock.read();
    assert_eq!(*r, 0);

    assert!(
        lock.try_write_or(move || second2.store(true, Relaxed))
            .is_none()
    );
    assert!(!second.load(Relaxed));
    drop(r);
    assert!(second.load(Relaxed));
}

#[test]
fn test_not_dropping_unblocked_after_read_guard_drain() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r = lock.read();
    assert!(lock.try_write_or(|| {}).is_none());
    drop(r);

    let r2 = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );
    drop(r2);

    assert!(called.load(Relaxed));
}

#[test]
fn test_write_then_read_then_write_cycle() {
    use parking_lot::Mutex;

    let lock = Arc::new(RwLockBell::new(0u64));
    let order = Arc::new(Mutex::new(Vec::<&str>::new()));

    // Phase 1: write guard fires callback
    {
        let o = order.clone();
        let w = lock.write();
        assert!(lock.try_write_or(move || o.lock().push("write")).is_none());
        drop(w);
    }

    // Phase 2: read guard fires callback
    {
        let o = order.clone();
        let r = lock.read();
        assert!(lock.try_write_or(move || o.lock().push("read")).is_none());
        drop(r);
    }

    // Phase 3: write guard fires callback again
    {
        let o = order.clone();
        let w = lock.write();
        assert!(lock.try_write_or(move || o.lock().push("write2")).is_none());
        drop(w);
    }

    assert_eq!(*order.lock(), vec!["write", "read", "write2"]);
}

#[test]
fn test_read_guard_during_write_callback() {
    let lock = Arc::new(RwLockBell::new(0u64));
    let lock2 = lock.clone();
    let inner_read = Arc::new(AtomicBool::new(false));
    let inner_read2 = inner_read.clone();

    let w = lock.write();
    assert!(
        lock.try_write_or(move || {
            // The write lock is released before callbacks fire,
            // so we should be able to read.
            let r = lock2.read();
            assert_eq!(*r, 0);
            inner_read2.store(true, Relaxed);
        })
        .is_none()
    );
    drop(w);
    assert!(inner_read.load(Relaxed));
}
