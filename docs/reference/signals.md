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

Use `camber::runtime::request_shutdown()` to begin graceful shutdown from application code. It latches runtime shutdown and closes root-scope admission in one step, so a `spawn` or `spawn_async` after it is refused with `RuntimeError::ScopeClosed`.

Use `camber::on_cancel(future)` to register an external shutdown trigger. When that future resolves, Camber treats it as a shutdown request.

## OS Signals

`camber::signals::spawn_signal_watcher(shutdown, notify)` is the low-level helper that waits for:

- `SIGINT` (`Ctrl-C`)
- `SIGTERM`

Both are registered through Tokio's Unix signal API, synchronously, before the watcher is spawned — a signal raised immediately afterwards still reaches an installed handler. A registration that fails is logged and the watcher simply never wakes on that source; it is not an error the caller sees.

On either signal it sets the shared `shutdown` flag and wakes the waiters on `notify`.

It needs a Tokio runtime on the calling thread and does not degrade without one: signal registration and the spawn both panic. Call it from inside `runtime::run`, or from any other Tokio runtime context.

The returned `JoinHandle` has two completion paths, and only one of them touches the flag:

| Completion path | `shutdown` flag |
|---|---|
| SIGINT or SIGTERM arrives | set to `true`, waiters notified |
| the watcher's runtime closes its root scope or latches shutdown | untouched |

The second path exists because the watcher captures the lifecycle signals of the Camber runtime it was spawned in. An OS signal is an external edge that may never arrive; without a lifecycle exit arm, a watcher spawned inside a runtime would outlive the scope that has to drain. Spawned outside a Camber runtime, the watcher observes signals that nothing can fire, so only an OS signal ends it.

**Read the flag; do not infer it from completion.** A caller that awaits the handle and treats "the handle resolved" as "an OS signal arrived" is wrong inside a runtime. Check `shutdown.load(Ordering::Acquire)` after the join.

Most applications do not need to call this directly. Camber's runtime setup handles signal watching for the normal server and runtime entrypoints, and that watcher does more than this one: it applies the whole shutdown request, so a SIGINT or SIGTERM closes root-scope admission exactly as `request_shutdown()` does. This low-level helper owns no runtime, so it fires the caller's latch alone.

## Typical Shape

- use `http::serve(...)` or `runtime::builder().run(...)`
- let Camber install signal handling
- optionally call `request_shutdown()` yourself for programmatic shutdown
- use `on_shutdown().await` in background tasks that need cleanup work
