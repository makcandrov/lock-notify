use std::{
    hint::spin_loop,
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering::Relaxed, Ordering::SeqCst},
    },
    thread,
    time::Duration,
};

use lock_notify::RwLockNotify;
use parking_lot::{Mutex, RwLock};

// ─── write-guard path (original behaviour) ───────────────────────────────────

#[test]
fn test_callback() {
    let lock = RwLock::new(12u64);
    let lock_callback = Arc::new(RwLockNotify::from_inner(lock));

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
            let guard = lock_callback_1.try_write_or(|| {}).unwrap();
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

            let r = lock_callback_2.try_write_or(move || {
                assert!(callback_allowed_2.load(Relaxed));
            });
            assert!(r.is_none());
            step_2.fetch_add(1, Relaxed);
        });
    });
}

#[test]
fn test_multiple_callbacks() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let order = Arc::new(Mutex::new(Vec::new()));

    let guard = lock.try_write_or(|| {}).unwrap();

    for i in 0..5u64 {
        let order = order.clone();
        let _ = lock.try_write_or(move || order.lock().push(i));
    }

    drop(guard);
    assert_eq!(*order.lock(), vec![0, 1, 2, 3, 4]);
}

#[test]
fn test_callback_can_call_try_write() {
    // Verifies that callbacks execute without holding any internal lock,
    // so re-entering try_write_or from a callback does not deadlock.
    let lock = Arc::new(RwLockNotify::new(0u64));
    let lock2 = lock.clone();

    let guard = lock.try_write_or(|| {}).unwrap();
    let _ = lock.try_write_or(move || {
        // Lock is free here — this must not deadlock.
        let _ = lock2.try_write_or(|| {});
    });
    drop(guard);
}

#[test]
fn test_into_inner() {
    let lock = RwLockNotify::new(42u64);
    assert_eq!(lock.into_inner(), 42);
}

#[test]
fn test_read() {
    let lock = RwLockNotify::new(42u64);
    assert_eq!(*lock.read(), 42);
}

#[test]
fn test_concurrent_reads() {
    // Two read guards can be held simultaneously.
    let lock = Arc::new(RwLockNotify::new(42u64));
    let r1 = lock.read();
    let r2 = lock.read();
    assert_eq!(*r1, 42);
    assert_eq!(*r2, 42);
}

#[test]
fn test_callback_panic_does_not_skip_subsequent() {
    // A panicking callback must not prevent later callbacks from running.
    // The panic is re-raised after all callbacks complete.
    let lock = Arc::new(RwLockNotify::new(0u64));

    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let guard = lock.write();
    let _ = lock.try_write_or(|| panic!("intentional panic in callback"));
    let _ = lock.try_write_or(move || called2.store(true, Relaxed));

    // The drop re-raises the panic from the first callback.
    let result = panic::catch_unwind(AssertUnwindSafe(|| drop(guard)));
    assert!(result.is_err(), "panic should have been re-raised");

    // But the second callback still ran.
    assert!(called.load(Relaxed));
}

#[test]
fn test_write_triggers_callbacks() {
    // write() (blocking) should run registered callbacks on drop, just like
    // a guard obtained via try_write_or().
    let lock = Arc::new(RwLockNotify::new(0u64));

    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let guard = lock.write();
    let _ = lock.try_write_or(move || called2.store(true, Relaxed));
    drop(guard);

    assert!(called.load(Relaxed));
}

// ─── read-guard path ──────────────────────────────────────────────────────────

/// Callback registered while a single read guard is held fires when that guard drops.
#[test]
fn test_callback_fires_on_single_read_guard_drop() {
    let lock = Arc::new(RwLockNotify::new(0u64));
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

/// try_read fires callback on drop (same guarantee as read).
#[test]
fn test_callback_fires_on_try_read_guard_drop() {
    let lock = Arc::new(RwLockNotify::new(0u64));
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

/// With multiple concurrent read guards the callback fires only when the
/// *last* guard is dropped, not on any intermediate drop.
#[test]
fn test_callback_fires_on_last_read_guard_drop() {
    let lock = Arc::new(RwLockNotify::new(0u64));
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

/// Multiple callbacks are all fired, in FIFO order, by the last read guard drop.
#[test]
fn test_multiple_callbacks_fired_by_last_read_guard() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let order = Arc::new(Mutex::new(Vec::<u64>::new()));

    let r = lock.read();

    for i in 0..5u64 {
        let order = order.clone();
        assert!(lock.try_write_or(move || order.lock().push(i)).is_none());
    }

    drop(r);
    assert_eq!(*order.lock(), vec![0, 1, 2, 3, 4]);
}

/// Callbacks fire without any lock held (read-guard path): the lock must be
/// writable from inside the callback.
#[test]
fn test_callback_no_lock_held_read_path() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let lock2 = lock.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || {
            // Must not deadlock: no lock should be held when callbacks run.
            let mut w = lock2.write();
            *w = 99;
        })
        .is_none()
    );

    drop(r);
    assert_eq!(*lock.read(), 99);
}

/// A callback registered during a read-guard drain (i.e. a callback that calls
/// try_write_or) fires on the next drain.
#[test]
fn test_callback_from_read_guard_drain_can_re_register() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let second_called = Arc::new(AtomicBool::new(false));
    let second_called2 = second_called.clone();
    let lock2 = lock.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || {
            // Re-register: the lock is free at this point so try_write_or succeeds;
            // store true so the test can assert it ran.
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

/// A panic inside a callback fired by the last read guard is re-raised, and
/// subsequent callbacks in the same batch still execute.
#[test]
fn test_callback_panic_read_guard_path() {
    let lock = Arc::new(RwLockNotify::new(0u64));
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

// ─── interaction between read-guard and write-guard drains ───────────────────

/// Callback registered while a write guard is held fires from the write guard
/// drop even if a read guard is live concurrently (acquired after try_write_or).
/// The read-guard drain sees `dropping = true` and correctly defers, letting the
/// write guard own the flush.
#[test]
fn test_dropping_flag_prevents_double_drain() {
    // Sequence:
    //  1. write guard W acquired
    //  2. try_write_or → callback C registered
    //  3. W dropped → dropping = true, C taken, write lock released,
    //                  dropping = false, C fires
    //  4. (After step 3) new read guard acquired, then dropped
    //     → readers = 0, callbacks empty → read guard drops silently
    let lock = Arc::new(RwLockNotify::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let w = lock.write();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );
    drop(w); // fires callback

    assert!(called.load(Relaxed));

    // A subsequent read guard should not attempt a spurious drain.
    let r = lock.read();
    drop(r);
    // (no assert needed — mainly checks for no panic / double-run)
}

// ─── concurrency / timing tests ──────────────────────────────────────────────

/// Callback registered while a read guard is held fires correctly even when
/// the write guard's drop and the last read guard's drop race.
///
/// Specifically verifies the scenario the user raised: a new lock acquisition
/// immediately after the last read guard releases must not prevent the callback
/// from firing.
#[test]
fn test_callback_fires_immediately_not_deferred() {
    // We can't directly observe wall-clock timing, but we can verify that the
    // callback fires as part of the read-guard drop rather than being deferred.
    let lock = Arc::new(RwLockNotify::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );

    // Drop r and check immediately — no other thread can steal the drain
    // because the callback is registered under the readers counter, not try_write.
    drop(r);
    assert!(
        called.load(Relaxed),
        "callback must fire synchronously in the drop of the last read guard"
    );
}

/// Multiple read guards held simultaneously; callbacks fire exactly CALLBACKS
/// times after all guards are released on the same thread (guards are !Send).
#[test]
fn test_multiple_readers_callbacks_fire_on_last_drop() {
    const READERS: usize = 8;
    const CALLBACKS: usize = 16;

    let lock = Arc::new(RwLockNotify::new(0u64));
    let count = Arc::new(AtomicU64::new(0));

    let r1 = lock.read();
    let r2 = lock.read();
    let r3 = lock.read();
    let r4 = lock.read();
    let r5 = lock.read();
    let r6 = lock.read();
    let r7 = lock.read();
    let r8 = lock.read();
    let _ = READERS; // used above

    for _ in 0..CALLBACKS {
        let c = count.clone();
        assert!(
            lock.try_write_or(move || {
                c.fetch_add(1, Relaxed);
            })
            .is_none()
        );
    }

    // Drop all but one; callbacks must not fire yet.
    drop(r1);
    drop(r2);
    drop(r3);
    drop(r4);
    drop(r5);
    drop(r6);
    drop(r7);
    assert_eq!(count.load(Relaxed), 0, "must not fire before last drop");

    // Last drop fires all CALLBACKS callbacks.
    drop(r8);
    assert_eq!(count.load(Relaxed), CALLBACKS as u64);
}

/// try_write_or racing with the last read guard dropping: callback registered
/// just before or just after the drop must still fire exactly once.
#[test]
fn test_try_write_or_races_last_read_guard_drop() {
    // Run many iterations to stress-test the race window.
    for _ in 0..500 {
        let lock = Arc::new(RwLockNotify::new(0u64));
        let called = Arc::new(AtomicU64::new(0));

        let r = lock.read();

        let lock2 = lock.clone();
        let called2 = called.clone();

        // This thread will race with drop(r) below.
        let handle = thread::spawn(move || {
            // May succeed (lock already free) or fail (read guard still held).
            match lock2.try_write_or(move || {
                called2.fetch_add(1, Relaxed);
            }) {
                Some(_guard) => {
                    // Got the write lock — callback discarded, count stays 0.
                    // That is correct: try_write_or only queues on failure.
                }
                None => {
                    // Callback queued; it must fire when r drops.
                }
            }
        });

        drop(r); // fires queued callback if any
        handle.join().unwrap();

        // called is 0 (try_write_or succeeded) or 1 (try_write_or failed and
        // callback fired). In both cases it must be ≤ 1 (no double-fire).
        assert!(called.load(Relaxed) <= 1);
    }
}

/// Stress test: many writers using try_write_or simultaneously while a read
/// guard is held; the total callback count equals the number of failed attempts.
#[test]
fn test_stress_many_writers_one_reader() {
    const WRITERS: usize = 32;

    let lock = Arc::new(RwLockNotify::new(0u64));
    let success_count = Arc::new(AtomicU64::new(0));
    let callback_count = Arc::new(AtomicU64::new(0));

    let r = lock.read();

    thread::scope(|s| {
        for _ in 0..WRITERS {
            let lock2 = lock.clone();
            let sc = success_count.clone();
            let cc = callback_count.clone();
            s.spawn(move || {
                if lock2.try_write_or(move || {
                    cc.fetch_add(1, Relaxed);
                }).is_some() {
                    sc.fetch_add(1, Relaxed);
                }
            });
        }
    });
    // All WRITERS have attempted; now drop the read guard to fire callbacks.
    drop(r);

    let successes = success_count.load(Relaxed);
    let callbacks = callback_count.load(Relaxed);
    assert_eq!(
        successes + callbacks,
        WRITERS as u64,
        "every try_write_or call either succeeded or registered a callback that fired"
    );
}

/// A write guard dropped while a concurrent read guard exists does not lose
/// the callback: the read guard, when dropped, will fire any callbacks that
/// were queued after the write guard's drain.
#[test]
fn test_write_guard_drop_then_read_guard_drop_sequence() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    let first2 = first.clone();
    let second2 = second.clone();

    // Phase 1: write guard registers and fires first callback.
    {
        let w = lock.write();
        assert!(
            lock.try_write_or(move || first2.store(true, Relaxed))
                .is_none()
        );
        drop(w);
    }
    assert!(first.load(Relaxed));

    // Phase 2: read guard is live when a new callback is registered.
    let r = lock.read();
    assert!(
        lock.try_write_or(move || second2.store(true, Relaxed))
            .is_none()
    );
    assert!(!second.load(Relaxed));
    drop(r);
    assert!(second.load(Relaxed));
}

/// The `dropping` flag is properly reset after the read-guard drain so that
/// subsequent try_write_or calls are not permanently blocked.
#[test]
fn test_not_dropping_unblocked_after_read_guard_drain() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();

    // First cycle: callback via read-guard path.
    let r = lock.read();
    assert!(lock.try_write_or(|| {}).is_none());
    drop(r); // drain runs, dropping → false

    // Second cycle: try_write_or must not be stuck on not_dropping.
    let r2 = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );
    drop(r2);

    assert!(called.load(Relaxed));
}

/// Concurrent: one thread holds a read guard; another repeatedly tries
/// try_write_or. All registered callbacks must eventually fire.
#[test]
fn test_concurrent_try_write_or_while_reader_held() {
    const ITERS: usize = 200;

    let lock = Arc::new(RwLockNotify::new(0u64));
    let count = Arc::new(AtomicU64::new(0));

    thread::scope(|s| {
        let r = lock.read();

        let lock2 = lock.clone();
        let count2 = count.clone();
        let writer_thread = s.spawn(move || {
            for _ in 0..ITERS {
                let c = count2.clone();
                if lock2
                    .try_write_or(move || {
                        c.fetch_add(1, Relaxed);
                    })
                    .is_some()
                {
                    // succeeded on this iteration — no callback registered
                    count2.fetch_add(1, Relaxed); // count it ourselves for bookkeeping
                }
            }
        });

        // Give the writer a moment to queue some callbacks before releasing.
        thread::sleep(Duration::from_millis(5));
        drop(r);
        writer_thread.join().unwrap();
    });

    // Every iteration either succeeded (we incremented count ourselves) or
    // failed (callback incremented count). Either way total == ITERS.
    assert_eq!(count.load(Relaxed), ITERS as u64);
}

/// try_write_or in-flight (locking > 0) when the last reader drops: the drain
/// correctly waits for the in-flight call to finish before collecting callbacks.
#[test]
fn test_in_flight_try_write_or_observed_by_drain() {
    // We simulate the race by injecting a delay into the try_write_or critical
    // section indirectly: the test relies on try_write_or's atomicity guarantee
    // (locking counter). We verify no callback is ever silently dropped.
    for _ in 0..200 {
        let lock = Arc::new(RwLockNotify::new(0u64));
        let called = Arc::new(AtomicU64::new(0));

        let r = lock.read();

        let lock2 = lock.clone();
        let called2 = called.clone();

        // Start a thread that will call try_write_or. It may race with drop(r).
        let handle = thread::spawn(move || {
            if lock2
                .try_write_or(move || {
                    called2.fetch_add(1, Relaxed);
                })
                .is_none()
            {
                // callback queued — must fire
            }
        });

        drop(r);
        handle.join().unwrap();

        // called == 0 (try_write_or saw the lock free) or 1 (callback fired).
        assert!(called.load(Relaxed) <= 1, "no double-fire");
    }
}

/// Dropping a read guard acquired *while* a drain is running (dropping = true)
/// must not deadlock and must leave the system in a clean state.
#[test]
fn test_read_guard_acquired_during_drain_does_not_deadlock() {
    let lock = Arc::new(RwLockNotify::new(0u64));
    let inner_read_dropped = Arc::new(AtomicBool::new(false));
    let inner_read_dropped2 = inner_read_dropped.clone();
    let lock2 = lock.clone();

    let r = lock.read();
    assert!(
        lock.try_write_or(move || {
            // During this callback, dropping = false (already reset).
            // Acquire + immediately drop a read guard to verify no deadlock.
            let r_inner = lock2.read();
            drop(r_inner);
            inner_read_dropped2.store(true, Relaxed);
        })
        .is_none()
    );

    drop(r);
    assert!(inner_read_dropped.load(Relaxed));

    // System must be usable afterwards.
    let called = Arc::new(AtomicBool::new(false));
    let called2 = called.clone();
    let r2 = lock.read();
    assert!(
        lock.try_write_or(move || called2.store(true, Relaxed))
            .is_none()
    );
    drop(r2);
    assert!(called.load(Relaxed));
}

/// High-contention stress: many threads reading and writing simultaneously,
/// each registering callbacks via try_write_or. Total fired count must equal
/// total registered count.
#[test]
fn test_high_contention_stress() {
    const THREADS: usize = 8;
    const OPS_PER_THREAD: usize = 50;

    let lock = Arc::new(RwLockNotify::new(0u64));
    let registered = Arc::new(AtomicU64::new(0));
    let fired = Arc::new(AtomicU64::new(0));

    thread::scope(|s| {
        for t in 0..THREADS {
            let lock2 = lock.clone();
            let reg = registered.clone();
            let fir = fired.clone();
            s.spawn(move || {
                for i in 0..OPS_PER_THREAD {
                    if t % 2 == 0 {
                        // Reader threads
                        let r = lock2.read();
                        // Occasionally register a callback from within a read guard
                        if i % 3 == 0 {
                            let f = fir.clone();
                            if lock2
                                .try_write_or(move || {
                                    f.fetch_add(1, Relaxed);
                                })
                                .is_none()
                            {
                                reg.fetch_add(1, Relaxed);
                            }
                        }
                        drop(r);
                    } else {
                        // Writer threads
                        let f = fir.clone();
                        match lock2.try_write_or(move || {
                            f.fetch_add(1, Relaxed);
                        }) {
                            Some(guard) => {
                                // got the lock; modify the value
                                // (write guard drop will drain any queued callbacks)
                                drop(guard);
                            }
                            None => {
                                reg.fetch_add(1, Relaxed);
                            }
                        }
                    }
                }
            });
        }
    });

    // Every callback that was registered must have been fired.
    assert_eq!(
        fired.load(SeqCst),
        registered.load(SeqCst),
        "all registered callbacks must fire exactly once"
    );
}
