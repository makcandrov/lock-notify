use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering::Relaxed},
};

use lockbell::RwLockBell;

// ─── read guard map ──────────────────────────────────────────────────────────

#[test]
fn test_map_read_guard() {
    let lock = RwLockBell::new((1u64, 2u64));
    let mapped = lock.read().map(|t| &t.0);
    assert_eq!(*mapped, 1);
}

#[test]
fn test_map_read_guard_fires_callbacks() {
    let lock = Arc::new(RwLockBell::new((1u64, 2u64)));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let mapped = lock.read().map(|t| &t.1);
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    assert!(!called.load(Relaxed));
    drop(mapped);
    assert!(called.load(Relaxed));
}

#[test]
fn test_try_map_read_guard_success() {
    let lock = RwLockBell::new((1u64, 2u64));
    let mapped = lock.read().try_map(|t| Some(&t.0));
    assert!(mapped.is_ok());
    assert_eq!(*mapped.unwrap(), 1);
}

#[test]
fn test_try_map_read_guard_failure() {
    let lock = RwLockBell::new((1u64, 2u64));
    let r = lock.read();
    let result = r.try_map(|_| None::<&u64>);
    assert!(result.is_err());
    // The original guard is returned.
    let original = result.unwrap_err();
    assert_eq!(*original, (1, 2));
}

#[test]
fn test_try_map_read_guard_failure_still_fires_callbacks() {
    let lock = Arc::new(RwLockBell::new((1u64, 2u64)));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    let result = r.try_map(|_| None::<&u64>);
    let original = result.unwrap_err();
    assert!(!called.load(Relaxed), "callback must not fire yet");
    drop(original);
    assert!(called.load(Relaxed), "callback must fire after drop");
}

#[test]
fn test_try_map_or_err_read_guard_success() {
    let lock = RwLockBell::new((1u64, 2u64));
    let mapped = lock.read().try_map_or_err(|t| Ok::<_, ()>(&t.1));
    assert!(mapped.is_ok());
    assert_eq!(*mapped.unwrap(), 2);
}

#[test]
fn test_try_map_or_err_read_guard_failure() {
    let lock = RwLockBell::new((1u64, 2u64));
    let r = lock.read();
    let result = r.try_map_or_err(|_| Err::<&u64, _>("oops"));
    assert!(result.is_err());
    let (original, err) = result.unwrap_err();
    assert_eq!(err, "oops");
    assert_eq!(*original, (1, 2));
}

// ─── write guard map ─────────────────────────────────────────────────────────

#[test]
fn test_map_write_guard() {
    let lock = RwLockBell::new((1u64, 2u64));
    let mut mapped = lock.write().map(|t| &mut t.0);
    *mapped = 99;
    drop(mapped);
    assert_eq!(lock.read().0, 99);
}

#[test]
fn test_map_write_guard_fires_callbacks() {
    let lock = Arc::new(RwLockBell::new((1u64, 2u64)));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let mapped = lock.write().map(|t| &mut t.1);
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    assert!(!called.load(Relaxed));
    drop(mapped);
    assert!(called.load(Relaxed));
}

#[test]
fn test_try_map_write_guard_success() {
    let lock = RwLockBell::new((1u64, 2u64));
    let mapped = lock.write().try_map(|t| Some(&mut t.0));
    assert!(mapped.is_ok());
    let mut m = mapped.unwrap();
    *m = 42;
    drop(m);
    assert_eq!(lock.read().0, 42);
}

#[test]
fn test_try_map_write_guard_failure() {
    let lock = RwLockBell::new((1u64, 2u64));
    let w = lock.write();
    let result = w.try_map(|_| None::<&mut u64>);
    assert!(result.is_err());
    let original = result.unwrap_err();
    assert_eq!(*original, (1, 2));
}

#[test]
fn test_try_map_write_guard_failure_still_fires_callbacks() {
    let lock = Arc::new(RwLockBell::new((1u64, 2u64)));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let w = lock.write();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    let result = w.try_map(|_| None::<&mut u64>);
    let original = result.unwrap_err();
    assert!(!called.load(Relaxed));
    drop(original);
    assert!(called.load(Relaxed));
}

#[test]
fn test_try_map_err_write_guard_success() {
    let lock = RwLockBell::new((1u64, 2u64));
    let mapped = lock.write().try_map_err(|t| Ok::<_, ()>(&mut t.1));
    assert!(mapped.is_ok());
    let mut m = mapped.unwrap();
    assert_eq!(*m, 2);
    *m = 77;
    drop(m);
    assert_eq!(lock.read().1, 77);
}

#[test]
fn test_try_map_err_write_guard_failure() {
    let lock = RwLockBell::new((1u64, 2u64));
    let w = lock.write();
    let result = w.try_map_err(|_| Err::<&mut u64, _>("oops"));
    assert!(result.is_err());
    let (original, err) = result.unwrap_err();
    assert_eq!(err, "oops");
    assert_eq!(*original, (1, 2));
}
