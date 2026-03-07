# lockbell

[![Crates.io](https://img.shields.io/crates/v/lockbell)](https://crates.io/crates/lockbell)
[![docs.rs](https://img.shields.io/docsrs/lockbell)](https://docs.rs/lockbell)
[![License](https://img.shields.io/crates/l/lockbell)](https://github.com/makcandrov/lockbell#license)

An [`RwLock`] that rings back when contention clears.

[`RwLock`]: https://docs.rs/parking_lot/latest/parking_lot/type.RwLock.html

## The problem

[`RwLock::try_write`] returns [`None`] when the lock is contended — and gives you no way to know when it becomes free. You're left choosing between spinning, blocking, or silently dropping work.

[`RwLock::try_write`]: https://docs.rs/parking_lot/latest/parking_lot/type.RwLock.html#method.try_write

[`None`]: https://doc.rust-lang.org/std/option/enum.Option.html#variant.None

## The solution

```rust
use lockbell::RwLockBell;

let lock = RwLockBell::new(0u32);

// Holds the lock — any concurrent try_write_or will register a callback
let guard = lock.write();

// Fails: lock is held. Callback is queued.
lock.try_write_or(|| println!("lock is free now"));

// Dropping the guard fires all queued callbacks in FIFO order.
drop(guard);
```

When [`try_write_or`] fails, the callback is queued. Queued callbacks are flushed in FIFO order — without holding any lock — when either:

- a write guard is dropped, or
- the last active read guard is dropped (if callbacks were queued while readers held the lock).

[`try_write_or_else`] is also available when constructing the callback is expensive and should only happen on contention.

[`try_write_or`]: https://docs.rs/lockbell/latest/lockbell/struct.RwLockBell.html#method.try_write_or
[`try_write_or_else`]: https://docs.rs/lockbell/latest/lockbell/struct.RwLockBell.html#method.try_write_or_else

## Callback semantics

- Callbacks run **after** the lock is released, in registration order.
- A callback is a notification, not a lock acquisition — the lock may already be re-acquired by the time the callback fires.
- Callbacks must be `FnOnce() + Send + 'static`.
- A panicking callback does not prevent subsequent callbacks from running.

## Installation

```toml
[dependencies]
lockbell = "0.1"
```

## Use cases

- **Lazy invalidation**: a thread that loses the write race registers a callback to invalidate a local cache entry once the winner finishes.
- **Coalesced work**: multiple producers collapse concurrent write attempts into a single update, with each loser notified on completion.
- **Write-through coordination**: trigger downstream work (logging, metrics, replication) exactly once per write, regardless of contention.
