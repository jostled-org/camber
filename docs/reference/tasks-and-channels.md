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

- `.join()`
- `.cancel()`

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

Two coordination primitives are intentionally left to Tokio because wrapping
them adds indirection without meaningful DX improvement.

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
