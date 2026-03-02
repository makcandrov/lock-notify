# lock-notify

An [`RwLock`] wrapper that fires callbacks when a write guard is released.

## The problem

`RwLock::try_write` returns `None` when the lock is contended — and gives you no way to know when it becomes free. You're left choosing between spinning, blocking, or silently dropping work.

## The solution

```rust
use lock_notify::RwLockNotify;

let lock = RwLockNotify::new(0u32);

// Holds the lock — any concurrent try_write_or will register a callback
let guard = lock.write();

// Fails: lock is held. Callback is queued.
lock.try_write_or(|| println!("lock is free now"));

// Dropping the guard fires all queued callbacks in FIFO order.
drop(guard);
```

When `try_write_or` fails, the callback is queued. When the write guard is dropped, every queued callback is called in FIFO order — without holding any lock.

## Callback semantics

- Callbacks are called **after** the write lock is released, in registration order.
- A callback is a notification, not a lock acquisition — the lock may already be held again by the time it fires.
- Callbacks must be `FnOnce + Send + 'static`.
- A panic in a callback does not affect subsequent callbacks.

## Installation

```toml
[dependencies]
lock-notify = "0.1"
```

## Use cases

- **Lazy invalidation**: a thread that loses the write race registers a callback to invalidate a local cache entry once the winner finishes.
- **Coalesced work**: multiple producers collapse concurrent write attempts into a single update, with each loser notified on completion.
- **Write-through coordination**: a side channel to trigger downstream work (logging, metrics, replication) exactly once per write, regardless of contention.
