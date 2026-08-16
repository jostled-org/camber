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
            Ok::<(), RuntimeError>(())
        })?
}
```

`run` returns `Result<T, RuntimeError>` and never inspects `T`, so a closure that
returns its own `Result` nests. The `?` takes the runtime-level failure and
leaves the closure's result as `main`'s value.

Use `runtime::run(...)` when you want a scoped structured-concurrency context without HTTP serving:

```rust
use camber::{RuntimeError, runtime, spawn};

fn main() -> Result<(), RuntimeError> {
    runtime::run(|| {
        let value = spawn(|| expensive_work()).join()?;
        println!("{value}");
        Ok::<(), RuntimeError>(())
    })?
}
```

`join()` carries the same failures as any other spawn: `NoRuntime` outside a
runtime, `ScopeClosed` once admission has closed, `TaskPanicked` when the child
unwound.

## Return Values

- `runtime::run(...)` returns `Result<T, RuntimeError>`
- `RuntimeBuilder::run(...)` returns `Result<T, RuntimeError>`
- `http::serve(...)` returns `Result<(), RuntimeError>`

Camber library APIs do not exit the process. Binaries decide how to render or map errors.

A startup failure preempts all of this. Builder validation, the nested-runtime
check, TLS resolution, metrics installation, and executor construction all run
before your closure does, so any one of them returns its error without the
closure ever being called. Nothing below can displace it.

Once the closure runs, `run` never inspects its value. `T` is opaque to the
runtime, so it is either returned as `Ok(T)` or displaced whole by a
runtime-level failure. Those failures are ordered, first match wins:

0. your closure unwinding → the panic resumes, and nothing returns through the
   `Result` at all
1. a panic in a Camber-owned background child → `RuntimeError::TaskPanicked`
2. the scope drain expiring before every child exited on its own →
   `RuntimeError::ScopeDrainTimeout(count)`, where `count` is how many children
   the boundary found outstanding

A panicking closure does not escape teardown. Camber catches the unwind, runs
teardown in full — admission closes, `ScopeClosing` fires, the scope drains,
resources shut down — and resumes the panic on the far side, so your panic
semantics are unchanged. A recorded internal panic or a drain timeout is then
logged rather than returned: the payload leaves through the panic, so the
`Result` has no room left to carry either. Every failed assertion inside
`#[camber::test]` takes this path.

A panic outranks a drain timeout because a timeout is often the consequence of
a wedged panicking child, so reporting the panic points at the real fault.

A panic in a task **you** spawned with `camber::spawn` or `camber::spawn_async`
is delivered on that task's own handle and leaves the runtime result alone.
Panics never cancel sibling tasks.

## Runtime Builder

`runtime::builder()` configures the runtime before it starts.

Common options:

- `worker_threads(n)`
- `server_policy(policy)` — replaces the whole outer service envelope
- `shutdown_timeout(duration)`
- `header_timeout(duration)`
- `connection_limit(n)`
- `resource(...)`
- `health_interval(duration)`
- `otel_endpoint(url)` with the `otel` feature

`header_timeout(duration)` bounds Hyper's wait for a complete HTTP/1 request head. It
replaces `keepalive_timeout`, which never owned an idle keep-alive timer: the value has
always configured Hyper's HTTP/1 header-read boundary, and the name now states the
failure it produces. Hyper exposes no equivalent HTTP/2 per-stream partial-HEADERS
timer. Camber's HTTP/2 request deadlines begin after Hyper delivers a complete head.

`connection_limit(0)` is invalid and returns `RuntimeError::InvalidArgument` when the
runtime starts. Omitting the connection limit is unbounded — intended for development,
tests, or a service behind an admission boundary that already enforces one. A production
service should set a finite limit.

The runtime's policy is the outer ceiling for every server started inside it. A
`ServerBuilder` may narrow any dimension and can never widen one; under bare Tokio its
own `ServerPolicy` is the sole authority. The single-field setters above write onto the
one stored `ServerPolicy`, so the last write to a field wins and the others keep what
`server_policy` set. `shutdown_timeout` is the runtime's aggregate deadline: its servers
and its root scope consume the same duration rather than one each.

That deadline bounds Camber's own waiting and escalation. It cannot preempt an async task
that never yields, stop application code running on a blocking or OS thread, or prove that
an abandoned synchronous callback has returned.

`otel_endpoint(url)` installs the OTLP exporter as the global tracing subscriber.
Another subscriber may already hold that slot: `camber::logging::init_logging`, or a
stack your application installed. Then no span reaches the exporter, so `run` returns
`RuntimeError::Config` rather than start. See
[Logging](logging.md#with-otel_endpoint).

## Background Servers

If you already have a Tokio runtime and want to run Camber servers inside it, use the async and background server entrypoints:

- `http::serve_async(...)`
- `http::serve_async_tls(...)`
- `http::serve_async_hosts(...)`
- `http::serve_async_hosts_tls(...)`
- `http::serve_background(...)`
- `http::serve_background_tls(...)`
- `http::serve_background_hosts(...)`
- `http::serve_background_hosts_tls(...)`

All eight refuse synchronously when no Tokio executor is established:
`serve_async*` returns `Result<ServerHandleFuture, RuntimeError>` and
`serve_background*` returns `Result<ServerHandle, RuntimeError>`. Each is a thin
call onto `http::server(router)` or `http::server_hosts(hosts)`, which returns a
`ServerBuilder` that also accepts a `ServerPolicy` and a TLS configuration.

`ServerHandle`:

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

Every terminal requires an active Tokio runtime and captures its Camber or
plain-Tokio context synchronously, before it returns. A Camber call site
captures runtime shutdown, the containing `ServerPolicy`, and observability
state; the server's own policy narrows that envelope and can never widen it. A
plain-Tokio call site uses per-server control and its own `ServerPolicy` as the
sole authority, with no Camber task accounting.

`serve_async*` returns an owned future, not an `async fn`: routing is frozen and
the context captured before the future exists, so moving it to another executor
changes neither. Dropping it aborts its retained children; polling it to a ready
result is the completion proof.

### Runtime Shutdown Scope

`runtime::request_shutdown`, `on_cancel` completion, and an OS signal observed
by the active signal watcher request graceful server shutdown. Closure return
from `RuntimeBuilder::run` is not itself a server shutdown event. Retain and
await `ServerHandleFuture` before releasing resources when transport completion
must be proved.

The signal watcher belongs to the current Camber runtime. It is a root-scope
child that exits when the scope closes at closure return, and teardown awaits
that exit rather than aborting it. A signal received after the signal watcher
is gone has no server-lifecycle guarantee. Awaiting a server owner does not
extend runtime task waiting or recreate that watcher.

### Two Lifecycle Signals

Camber-owned background work observes two distinct events:

- **Scope closing** fires when the user closure returns, and also whenever
  shutdown is requested. It is what stops perpetual background children —
  interval and cron schedules, per-resource health loops, the proxy health
  checker, ACME/DNS-01 renewal, and the signal watcher.
- **Runtime shutdown** (`runtime::request_shutdown`, `on_cancel` completion, an
  OS signal) is *not* fired by closure return. The owned HTTP server observes
  only this one, which is why returning from the closure does not stop a server
  that is still serving.

### Teardown Order and `shutdown_timeout`

Teardown runs in one order: the closure returns, admission closes and scope
closing fires, the root scope drains, resources shut down, Tokio shuts down.

`shutdown_timeout` bounds the **scope drain**, not total return. Children get
that long to exit cooperatively. At the boundary Camber aborts every remaining
async child and joins it under one short forced-join grace, then returns
`RuntimeError::ScopeDrainTimeout(count)` reporting how many children the
boundary found outstanding. Aborting drops a task that is waiting at an
`.await`; a task that never awaits, and any `camber::spawn` blocking closure,
cannot be stopped — the drain reports it and proceeds instead of waiting.

Resource shutdown then runs unbounded — `shutdown_timeout` does not apply to it.
The final Tokio shutdown is bounded by a second, full `shutdown_timeout`: that
window belongs to whatever Tokio still carries once the scope has drained — a
server handle you dropped without joining, and the connection tasks its
supervisor was still ending. Worst-case return is therefore roughly
`2 × shutdown_timeout` plus however long resource shutdown takes.

A per-resource health task is a root-scope child, and `Resource::health_check`
runs under `block_in_place`, which `abort()` cannot preempt. A resource whose
health check blocks — a database ping with its own multi-second timeout, say —
while the drain escalates will not join within the forced-join grace, so the
scope reports it outstanding and `run` returns `RuntimeError::ScopeDrainTimeout`
where it previously returned `Ok`. Keep `health_check` bounded well inside
`shutdown_timeout`, or expect that result.

A forcibly aborted `spawn_async` task's own handle resolves
`RuntimeError::TaskPanicked("task channel closed")`. `RuntimeError::Cancelled`
is never a drain outcome. It is delivered only by explicit cancellation:
`AsyncJoinHandle::cancel()`, which drops the spawned future; `JoinHandle::cancel()`
on a `camber::spawn` blocking task, which the task observes at its next Camber IO
boundary; or `ServerHandle::cancel()`, which the server owner returns as its
terminal outcome.

### Outside a Runtime

`runtime::request_shutdown` and `runtime::on_cancel` are no-ops when no runtime
is established — there is nothing to shut down, and `on_cancel`'s future is
dropped unpolled. `camber::task::on_shutdown()` observes a signal that can never
fire, so it never completes. `camber::spawn` and `camber::spawn_async` run
nothing and their handles yield `RuntimeError::NoRuntime`.

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

fn main() -> Result<(), RuntimeError> {
    runtime::builder()
        .resource(Cache)
        .run(|| Ok::<(), RuntimeError>(()))?
}
```

Resources shut down concurrently — one thread per resource, every one joined
before `run` returns. There is no ordering guarantee between them, so a resource
must not depend on another still being up while it shuts down.

### Sync bridge: `runtime::block_on`

`Resource` trait methods (`health_check`, `shutdown`) are synchronous. When an implementation
must call async code — an async client, a message-queue callback — bridge with
`runtime::block_on`, which runs the future to completion via `block_in_place` +
`Handle::block_on` from inside a runtime worker:

```rust
fn health_check(&self) -> Result<(), RuntimeError> {
    runtime::block_on(async { self.client.ping().await })
}
```

`block_on` is for synchronous contexts only — `Resource` impls, message-queue callbacks, and
the synchronous CLI `run()` closure. Async handlers already have `.await` and must not use it.

## Error Handling

Camber uses one main error type at the runtime boundary: `RuntimeError`.

Common variants include:

- `Io` — an underlying I/O failure
- `Http` — an HTTP client, server, or protocol-level failure
- `Tls` — TLS setup or handshake failed
- `Timeout` — an operation exceeded its configured timeout
- `Cancelled` — cooperative cancellation was requested
- `TaskPanicked` — a spawned task unwound with a panic payload
- `InvalidArgument` — a public API was called with an invalid argument
- `NoRuntime` — a runtime-requiring entry point was called with no runtime context
- `ScopeClosed` — admission was attempted at or after the root scope's close transition
- `ScopeDrainTimeout(usize)` — the bounded scope drain expired, carrying how many children the boundary found outstanding (only the async subset of that count is then aborted and joined)

Use normal Rust error propagation with `?`.
