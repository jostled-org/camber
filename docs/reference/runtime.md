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

- can be cancelled with `.cancel()`
- can be awaited for `Result<(), RuntimeError>`

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
