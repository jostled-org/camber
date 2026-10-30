# Tasks and Channels Reference

Camber exposes task and channel APIs for structured IO-bound work.

## Tasks

### `spawn`

Use `spawn` for sync/blocking closures on Tokio's blocking pool:

```rust
use camber::spawn;

let handle = spawn(|| expensive_work());
let value = handle.join()?;
```

`JoinHandle<T>` supports:

- `.await`
- `.join()`
- `.cancel()`

In async code, await blocking work so the runtime worker stays available:

```rust
use camber::RuntimeError;

async fn count_words(input: Box<str>) -> Result<usize, RuntimeError> {
    camber::spawn(move || input.split_whitespace().count()).await
}
```

Both wait forms return the same result and errors: `NoRuntime`, `ScopeClosed`, or
`TaskPanicked`. `join()` blocks the calling thread. `.await` yields the worker until the result arrives.
Dropping the handle or its future discards the result. The runtime scope still owns
the closure until it finishes, because Camber cannot preempt blocking work.
To request cancellation at a cooperating Camber IO boundary, call `cancel()` before you await the handle.

The future type is `camber::task::JoinFuture`, beside `camber::task::AsyncJoinFuture`.

### `spawn_async`

Use `spawn_async` for async futures:

```rust
use camber::spawn_async;

let handle = spawn_async(async {
    do_async_work().await
});

let value = handle.await?;
```

`AsyncJoinHandle<T>` supports:

- `.await`
- `.cancel()`

## Structured Concurrency

Tasks spawned inside `runtime::run(...)` or `RuntimeBuilder::run(...)` are part of the runtime scope. Camber waits for them before returning.

## Fair Service Turns

Use `Turn` to share a limited service resource across requests. `Turn::new()`
admits one holder. `Turn::with_capacity(n)?` admits up to `n` holders and rejects
zero or values above Tokio's semaphore limit. Clones share capacity and closure.

`acquire().await` returns `Result<TurnGuard, RuntimeError>`. Hold the owned guard
for the resource operation; dropping it releases capacity. Admission follows
the order in which acquisition futures register, rather than HTTP arrival order.
An unpolled future has no queue position.

`queued()` counts acquisitions that returned `Pending` and have not completed.
This includes a reserved permit whose future has not yet delivered its guard.
It excludes holders and unpolled futures. Dropping a waiting future removes its
queue entry and releases any reserved capacity.

### Deadlines, cancellation, and closure

`acquire_until(deadline, cancel).await` accepts an optional absolute
`std::time::Instant` and a cancellation future with any output type. It returns
the same result type as `acquire()`. Ready cancellation wins over an expired
deadline; an expired deadline wins over a grant.

Refusals are `RuntimeError::Turn(TurnRefused::Cancelled)`, `Deadline`, or `Closed`.
`close()` stops admission and wakes queued callers. Closure affects every clone;
`is_closed()` reports it. Delivered guards remain valid until dropped.

Untimed acquisition needs no runtime. A pending timed wait needs Tokio's timer
driver, as `tokio::time::sleep` does. Without Tokio, it returns `NoRuntime`. On a
Tokio runtime with the timer driver disabled, it panics inside Tokio. Camber
runtimes always enable the timer driver.

Ready cancellation, expiry, closure, and immediate grants need no timer. A
refusal leaves no queue entry behind.

Cancellation is explicit. Pass `camber::on_shutdown()` to stop waiting when the
Camber runtime begins shutdown. Without a Camber runtime, `on_shutdown()` remains
pending. A peer disconnect drops the handler future, and the dropped acquisition
leaves the queue. [HTTP admission examples](http.md#waiting-for-a-service-turn)
show request work, typed refusal answers, and a heartbeat loop.

### Keep the guard with blocking work

Move the guard into the closure when blocking work owns the resource. Dropping
the result waiter does not stop that closure. Releasing its guard earlier could
admit another operation while the first one still runs.

```rust,no_run
use camber::{RuntimeError, Turn};

async fn sort_with_turn(turn: &Turn, mut values: Vec<u64>) -> Result<Vec<u64>, RuntimeError> {
    let guard = turn.acquire_until(None, camber::on_shutdown()).await?;
    camber::spawn(move || {
        values.sort_unstable();
        drop(guard);
        values
    })
    .await
}
```

Call this function inside a Camber runtime so `spawn` can admit the closure.
`Turn` creates no background task and does not cancel admitted work. The work
owner decides when to release its guard.

## Channels

```rust
use camber::channel;

let (tx, rx) = channel::new::<String>();
let (tx, rx) = channel::bounded::<String>(10);
```

Properties:

- bounded by default
- backpressure on send
- cloneable receiver for MPMC-style consumption

Common methods:

- `tx.send(value)?`
- `rx.recv()?`
- `rx.iter()`

## Async MPSC

Use `channel::mpsc` when you want async receive semantics for future composition.

## `select!`

For sync channel coordination, use `camber::select!`:

```rust
camber::select! {
    val = rx1 => { println!("rx1: {:?}", val); },
    val = rx2 => { println!("rx2: {:?}", val); },
    timeout(std::time::Duration::from_secs(1)) => { println!("timeout"); },
}
```

Use Tokio's own `tokio::select!` inside async code when you are waiting on futures.

## Watch Channel

Use `channel::watch` when multiple receivers need the latest value of a shared state
(configuration, shutdown signal, view-model snapshot). Receivers never queue stale
values — they always see the most recent write.

```rust
use camber::channel::watch;

let (tx, rx) = watch(AppConfig::default());

// Writer: update config
tx.send(new_config)?;

// Reader: get latest value (non-blocking)
let current = rx.borrow().clone();
```

`WatchReceiver` supports async waiting:

```rust
let mut rx = rx.clone();
camber::spawn_async(async move {
    while rx.changed().await.is_ok() {
        let config = rx.borrow().clone();
        apply_config(&config);
    }
});
```

Methods:

- `WatchSender::send(value)` — replace current value; returns `ChannelClosed` if all receivers dropped
- `WatchSender::send_modify(|v| ...)` — mutate in place; always succeeds (sender owns the value)
- `WatchSender::borrow()` — read current value
- `WatchSender::clone()` — multiple senders can write to the same channel
- `WatchReceiver::borrow()` — read current value (does not mark as seen)
- `WatchReceiver::borrow_and_update()` — read and mark as seen
- `WatchReceiver::changed().await` — wait for next update
- `WatchReceiver::has_changed()` — check if value changed since last `changed()` or `borrow_and_update()`
- `WatchReceiver::clone()` — each clone tracks "seen" state independently

## Primitives That Stay Direct Tokio

Use Tokio directly for locks and channel operations that need no service
admission policy.

### `tokio::sync::Mutex` and `RwLock` — Shared Data

Use these locks to protect shared data. `Turn` controls access to a service
resource; it does not wrap or expose protected data. Keep using Tokio's semaphore
when you need its lower-level permit operations.

### `tokio::sync::broadcast` — Multi-Subscriber Event Fanout

Use `broadcast` when multiple subscribers each need their own copy of every event. Unlike
`watch`, broadcast delivers all values — not just the latest. Subscribers that fall behind
receive a `RecvError::Lagged` with the number of missed messages.

```rust
use tokio::sync::broadcast;

let (tx, _) = broadcast::channel::<Event>(64);

// Each subscriber gets its own receiver
let mut rx1 = tx.subscribe();
let mut rx2 = tx.subscribe();

// Publisher
tx.send(Event::Updated).ok();

// Subscriber
camber::spawn_async(async move {
    while let Ok(event) = rx1.recv().await {
        handle_event(event);
    }
});
```

### `tokio::sync::oneshot` — Single-Use Response Delivery

Use `oneshot` for request/response coordination between tasks — one sender, one receiver,
one value.

```rust
use tokio::sync::oneshot;

let (tx, rx) = oneshot::channel();

camber::spawn_async(async move {
    let result = compute().await;
    tx.send(result).ok();
});

let value = rx.await.map_err(|_| RuntimeError::ChannelClosed)?;
```
