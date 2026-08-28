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
runtime-level failure. One value carries every such failure:
`RuntimeError::Lifecycle`, the immutable account of each direct runtime-owned
participant that could not finish — a panicking Camber-owned background child,
a scope drain that expired with children still outstanding, a resource, the
exporter. Nothing weighs those against each other: the account keeps them all
and elects none of them. A caller reads the whole collection. See
[One aggregate shutdown deadline](#one-aggregate-shutdown-deadline) and
[Errors](error.md#lifecycle-aggregates).

The participant vocabulary is closed and direct:
root scope, background task, resource, and exporter.
These are the owners the runtime itself waits for and
authoritatively settles. A server, one of its connections, and an upgrade past
its response head settle inside the flat server tree that owns them and never
appear as runtime aggregate participants — a server's outcome reaches you
through its own `ServerHandleFuture`, not through this account. The Tokio
executor is not a participant either: Camber gets no acknowledgement back from
it, so there is no fact about it this vocabulary could honestly state.

`LifecycleParticipant::Exporter` is settlement-only vocabulary. The trace
provider's shutdown is unbounded and hands nothing back, so teardown settles it
completed through `ShutdownOwner::EXPORTER` and has no outcome it could record a
failure from. It names the owner the settlement inventory visited, and it is
never reached as an aggregate failure entry.

Entries are frozen in a stable rendering order — root scope, background
children, resources, then the exporter — with recording order deciding the
sequence inside one class. That order is reproducible output and nothing else.
It is not causal precedence, and the first entry is not more responsible than
the last.

Your closure unwinding sits above the `Result` rather than inside it. The panic
resumes, and nothing returns through the `Result` at all.

A panicking closure does not escape teardown. Camber catches the unwind, runs
teardown in full — admission closes, `ScopeClosing` fires, the scope drains,
resources shut down — and resumes the panic on the far side, so your panic
semantics are unchanged. The account teardown produced has no return path left,
so it leaves through one `lifecycle failures displaced by an unwinding closure`
event instead of being dropped. Every failed assertion inside `#[camber::test]`
takes this path.

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
runtime starts. So is a limit larger than the admission semaphore can hold; the error
names that ceiling. Omitting the connection limit is unbounded — intended for development,
tests, or a service behind an admission boundary that already enforces one. A production
service should set a finite limit.

With the `profiling` feature, `ServerPolicy::profiling_response_limit(max_bytes)` caps the
rendered profile `/debug/pprof/cpu` retains. It defaults to eight MiB. Sampling and
rendering run on a blocking thread, and each write is counted before it is kept: the write
that would carry the answer past the maximum is dropped, and the request is refused with
`RuntimeError::LimitExceeded` naming `ByteBoundary::ProfilingResponse` rather than answered
with a partial profile. The peer reads a redacted 500; the operator's record names the
bound. Zero is refused. `unbounded_profiling_response()` is the only spelling that removes
the maximum, and it holds the whole rendered answer in memory however large the sampler
made it.

`ServerPolicy` refuses every deadline that is zero or longer than thirty years, and the
request, transfer, and proxy budgets refuse the same two. Thirty years is the horizon
Tokio's own timer stops at. Past it Camber would hand the clock a deadline the platform
cannot represent. Spell "no deadline" with the unbounded constructor the dimension names,
never with a very large duration.

The builder's `shutdown_timeout(duration)` and `header_timeout(duration)` differ at the
low end. Each raises a duration under 100 ms to 100 ms and logs a warning. Zero through
the builder therefore starts the runtime with a 100 ms deadline instead of failing it.
Past thirty years both refuse like the policy setters, and `run` returns
`RuntimeError::InvalidArgument` naming the dimension. Set the deadline on a `ServerPolicy`
when you want zero refused rather than raised.

The runtime's policy is the outer ceiling for every server started inside it. A
`ServerBuilder` may narrow any dimension and can never widen one; under bare Tokio its
own `ServerPolicy` is the sole authority. The single-field setters above write onto the
one stored `ServerPolicy`, so the last write to a field wins and the others keep what
`server_policy` set. `shutdown_timeout` is the runtime's aggregate deadline: its servers
and its root scope consume the same duration rather than one each.

### One aggregate shutdown deadline

The first graceful transition anywhere — `runtime::request_shutdown()`, a signal the
watcher delivers, `ServerHandle::shutdown()`, a server's own fatal drain, or the `run`
closure returning — mints one absolute expiry from `shutdown_timeout`. Every later
transition reads that same instant back. No nested owner restarts it: a server, an
admitted request, a registered upgrade, the root scope, a registered resource, and the
Tokio executor all narrow one expiry instead of each starting a fresh copy of the grace.

A participant may be narrower and never wider. A `ServerBuilder` inside the runtime
narrows `shutdown_timeout` like any other dimension, and a resource callback receives the
smaller of its `ResourceBudget` phase deadline and the time the aggregate has left. No
participant is cut below the fixed 100 ms forced-join grace, which is what a forced stop
gives an owner it has just told to end.

`ServerHandle::cancel()` mints nothing. Explicit cancellation is forced termination now:
that server stops under the forced-join grace rather than under a fresh grace period,
whatever the aggregate had left.

Teardown returns one immutable account. `RuntimeBuilder::run` answers
`RuntimeError::Lifecycle` when any framework-owned participant could not finish, frozen
after every join or abandonment decision has been taken — see
[Errors](error.md#lifecycle-aggregates). A server's own result stays flat on its handle.

That deadline bounds Camber's own waiting and escalation. Cooperative cancellation cannot
preempt an async task that never yields, stop application code running on a blocking or OS
thread, or prove that an abandoned synchronous callback has returned. A participant Camber
could not prove finished is named in the returned aggregate rather than reported stopped.

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

### What the flat result means

The result is causal, not ranked. Each stop command commits its phase into the
server's stop state before it publishes anything, so a command that has returned
has already decided how the server ends and nothing downstream can change it.
`.cancel()` returning means forced cancellation is committed: the join reports
`RuntimeError::Cancelled` unless a deadline or a settlement committed first, in
which case that first commit keeps its own result and the cancellation is a
no-op. A graceful drain that runs out of its one aggregate deadline reports
`RuntimeError::Timeout`. Two facts with no commit order between them are not
ranked against each other; whichever committed first is the answer.

A successful join proves each owned accepted transport, connection permit, and
registered upgrade completed. The server registry owns only connections; each
connection owns its request and upgrade children; each upgrade retains its
directions and its callback. A connection permit is released by the
connection owner that holds it, after both children have settled — so a permit
outstanding past a join is a bug, not a race.

An upgrade's blocking callback is the one child Camber cannot force. Every
bridge terminal closes the callback-facing endpoints so a cooperative callback
wakes, and the upgrade owner holds one join deadline for it that a later server
transition may shorten but never restart. A callback still blocked at that
deadline is reported through one WARN event,
`camber.websocket.callback.outstanding`, carrying
`disposition="outstanding-after-forced-grace"`. Grep for that event and that
field: the `CallbackDisposition::OutstandingAfterForcedGrace` record behind them
is private to the bridge and is not a name to look up. Camber stops claiming the
callback returned, and the public server result still follows the accepted
server command.

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
  interval and cron schedules, the resource health coordinator, the proxy
  health checker, ACME/DNS-01 renewal, and the signal watcher.
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

Resource shutdown then runs under its own bound: each resource gets the
`ResourceBudget` shutdown deadline, visited one at a time in reverse
registration order. The final Tokio shutdown is bounded by a second, full
`shutdown_timeout`: that window belongs to whatever Tokio still carries once the
scope has drained — a server handle you dropped without joining, and the
connection tasks its supervisor was still ending.

One root-scope child runs every periodic health pass, and it abandons a probe it
is still waiting on when the scope closes. So a resource whose health check
blocks does not hold the drain open — but it does keep the resource, so its
teardown callback is reported as blocked rather than being called beside the
probe that never returned. Keep `health_check` bounded well inside its
configured deadline, or expect that result.

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

One coordinator visits every resource. The readiness pass and each periodic
health pass go in registration order; teardown goes in reverse registration
order, so a resource may depend on one registered before it still being up while
it shuts down. One resource never has two callbacks running at once.

Every callback runs on its own worker under the `ResourceBudget` deadline for
its phase. See the [Resource Reference](resource.md) for the budget, the
`/health` projection, and how each failure reaches a caller.

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
