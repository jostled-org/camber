# Signals And Shutdown Reference

Camber has two related concepts:

- runtime shutdown observation
- OS signal integration

## Observing Shutdown

Use `camber::on_shutdown()` inside spawned tasks to wait until the runtime begins shutting down:

```rust
use camber::{on_shutdown, spawn_async};

let handle = spawn_async(async {
    on_shutdown().await;
    cleanup_async().await;
});
```

Use `camber::runtime::is_shutting_down()` when you need a synchronous check.

## Requesting Shutdown

Use `camber::runtime::request_shutdown()` to begin graceful shutdown from application code.

Use `camber::on_cancel(future)` to register an external shutdown trigger. When that future resolves, Camber treats it as a shutdown request.

## OS Signals

`camber::signals::spawn_signal_watcher(shutdown, notify)` is the low-level helper that waits for:

- `Ctrl-C`
- `SIGTERM` on Unix

When triggered, it sets the shared shutdown flag and wakes waiters.

Most applications do not need to call this directly. Camber's runtime setup handles signal watching for the normal server and runtime entrypoints.

## Typical Shape

- use `http::serve(...)` or `runtime::builder().run(...)`
- let Camber install signal handling
- optionally call `request_shutdown()` yourself for programmatic shutdown
- use `on_shutdown().await` in background tasks that need cleanup work
