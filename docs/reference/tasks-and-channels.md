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
