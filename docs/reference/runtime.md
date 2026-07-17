# Runtime Reference

Camber runs on Tokio and exposes two public runtime styles:

1. `http::serve(...)` for the default HTTP server case
2. `runtime::run(...)` / `runtime::builder().run(...)` for scoped work and runtime configuration

## Canonical Entrypoints

Use `http::serve(...)` by itself when you want to run a normal HTTP service:

```rust
use camber::RuntimeError;
use camber::http::{self, Response, Router};

fn main() -> Result<(), RuntimeError> {
    let mut router = Router::new();
    router.get("/hello", |_req| async { Response::text(200, "Hello, world!") });
    http::serve("0.0.0.0:8080", router)
}
```

Use `runtime::builder().run(...)` when you need runtime configuration such as worker counts, shutdown timeouts, registered resources, or OpenTelemetry export:

```rust
use camber::{RuntimeError, runtime};
use std::time::Duration;

fn main() -> Result<(), RuntimeError> {
    runtime::builder()
        .worker_threads(8)
        .shutdown_timeout(Duration::from_secs(10))
        .run(|| {
            // start services, background work, resources, etc.
            Ok(())
        })
}
```

Use `runtime::run(...)` when you want a scoped structured-concurrency context without HTTP serving:

```rust
use camber::{runtime, spawn};

runtime::run(|| {
    let handle = spawn(|| expensive_work());
    let value = handle.join().unwrap();
    println!("{value}");
}).unwrap();
```

## Return Values

- `runtime::run(...)` returns `Result<T, RuntimeError>`
- `RuntimeBuilder::run(...)` returns `Result<T, RuntimeError>`
- `http::serve(...)` returns `Result<(), RuntimeError>`

Camber library APIs do not exit the process. Binaries decide how to render or map errors.

## Runtime Builder

`runtime::builder()` configures the runtime before it starts.

Common options:

- `worker_threads(n)`
- `shutdown_timeout(duration)`
- `keepalive_timeout(duration)`
- `connection_limit(n)`
- `resource(...)`
- `health_interval(duration)`
- `otel_endpoint(url)` with the `otel` feature

`connection_limit(0)` is invalid and returns `RuntimeError::InvalidArgument` when the runtime starts.

## Background Servers

If you already have a Tokio runtime and want to run Camber servers inside it, use the async and background server entrypoints:

- `http::serve_async(...)`
- `http::serve_async_tls(...)`
- `http::serve_async_hosts(...)`
- `http::serve_background(...)`
- `http::serve_background_hosts(...)`

Background server APIs return `ServerHandle`, which:

- requests graceful shutdown with `.shutdown()`
- requests forced cancellation with `.cancel()`
- transfers control without stopping admission with `.join()`
- combines graceful shutdown and transfer with `.shutdown_and_join()`
- can be awaited for a flat `Result<(), RuntimeError>`

`join`, `shutdown_and_join`, and awaiting the handle use the same concrete
`http::ServerHandleFuture`. That future retains `shutdown()` and `cancel()`
methods, so a caller may join first, continue serving, and request shutdown
later. Dropping either armed owner before completion requests forced shutdown;
polling a ready result disarms that behavior.

### Constructor Context

Each `serve_background*` constructor first requires an active Tokio runtime. It
then captures its Camber or plain-Tokio context synchronously before spawning.
A Camber call site captures runtime shutdown, configured timeout, connection
limit, keepalive, and observability state. A plain-Tokio call site uses
per-server control and the standalone shutdown and keepalive defaults, with no
Camber connection limit or task accounting.

Direct `serve_async*` functions classify context at first poll, not when their
future is created. Moving an unpolled direct future between contexts therefore
selects the destination polling context. Dropping a direct future aborts its
retained children but yields no join result; callers that need completion proof
use `ServerHandleFuture`.

### Runtime Shutdown Scope

`runtime::request_shutdown`, `on_cancel` completion, and an OS signal observed
by the active signal watcher request graceful server shutdown. Closure return
from `RuntimeBuilder::run` is not itself a server shutdown event. Retain and
await `ServerHandleFuture` before releasing resources when transport completion
must be proved.

The signal watcher belongs to the current Camber runtime and is aborted during
closure return. A signal received after the signal watcher is gone has no
server-lifecycle guarantee. Awaiting a server owner does not extend runtime
task waiting or recreate that watcher.

## Resource Lifecycle

Resources integrate external systems into runtime startup, health checks, and shutdown.

```rust
use camber::{Resource, RuntimeError, runtime};

struct Cache;

impl Resource for Cache {
    fn name(&self) -> &str { "cache" }
    fn health_check(&self) -> Result<(), RuntimeError> { Ok(()) }
    fn shutdown(&self) -> Result<(), RuntimeError> { Ok(()) }
}

runtime::builder()
    .resource(Cache)
    .run(|| Ok::<(), RuntimeError>(()))?;
```

Resources shut down in reverse registration order.

## Error Handling

Camber uses one main error type at the runtime boundary: `RuntimeError`.

Common variants include:

- `Io`
- `Http`
- `Tls`
- `Timeout`
- `Cancelled`
- `TaskPanicked`
- `InvalidArgument`

Use normal Rust error propagation with `?`.
