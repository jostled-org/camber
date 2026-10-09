# Error Reference

Camber uses one main error type at public API boundaries: `RuntimeError`.

Most top-level APIs return `Result<_, RuntimeError>`, so application code can use normal `?`
propagation without converting between framework-specific error types.

## Error Families

The variants cluster into a few stable buckets:

- runtime and coordination: `Io`, `Timeout`, `Cancelled`, `TaskPanicked`, `ChannelClosed`, `ChannelFull`, `BlockingInAsyncContext`
- configured service deadlines: `DeadlineExceeded(DeadlineBoundary)`
- configured byte maximums: `LimitExceeded(ByteBoundary)`
- runtime context and task lifecycle: `NoRuntime`, `ScopeClosed`, `ScopeDrainTimeout`, `Lifecycle`
- request and API misuse: `BadRequest`, `InvalidArgument`
- unparseable request payloads: `MalformedBody`, `Multipart`
- request payloads Camber refused to keep reading: `RequestBodyLimit`, `RequestBodyUnreadable`
- transport failures: `Http`, `Tls`
- managed integration failures: `Integration(Arc<IntegrationError>)`
- direct WebSocket termination: `WebSocketClosed` (behind the `ws` feature)
- application-supplied: `Database`
- startup and infrastructure configuration: `Config`, `Secret`, `Dns`, `Acme`, `Schedule`

The exact enum is documented in rustdoc. The useful public rule is that Camber keeps one shared error type across these surfaces so callers do not need framework-specific conversions.

## Runtime Context and Task Lifecycle

`spawn`, `spawn_async`, and the other runtime-requiring entry points report three outcomes that are not failures of the work itself:

- `NoRuntime` — the call was made with no runtime context established. Nothing was spawned. A handle returned before a runtime exists yields this on `join`/`.await`.
- `ScopeClosed` — a runtime exists, but its root scope has already closed to admission: the `run` closure returned, or shutdown was requested. Nothing was spawned. This is the defined disposition of a spawn inside the shutdown window, not a fault. In a handler it maps to `503`, not `500` — see [Handler Behavior](#handler-behavior).
- `ScopeDrainTimeout(n)` — the graceful drain expired with `n` children that had not exited on their own. `n` counts children that failed to exit cooperatively, not children still running when the entry point returned. It reaches a caller inside a `Lifecycle` aggregate rather than on its own.

`NoRuntime` and `ScopeClosed` are distinct so a caller can tell "no runtime at all" from "too late".

## Lifecycle Aggregates

`Lifecycle` is what a runtime entry point returns when teardown could not finish cleanly: `runtime::run`, `RuntimeBuilder::run`, `runtime::test`, or `#[camber::test]`. It carries every direct runtime-owned participant that failed during one startup or one teardown, frozen after every join or abandonment decision has been taken — never a single flattened winner, and never a chosen one.

```rust
use camber::{LifecycleFailureKind, RuntimeError};

fn report(error: &RuntimeError) {
    let RuntimeError::Lifecycle(failures) = error else { return };
    // Every direct owner that failed. None of them is the one to act on.
    for failure in failures.iter() {
        println!("{} failed in {}", failure.participant(), failure.phase());
        if let LifecycleFailureKind::Resource(resource) = failure.kind() {
            println!("resource {} : {}", resource.name(), resource.kind());
        }
    }
}
```

- `iter()` lists every entry in rendering order: root scope, background children, integrations, resources, then the exporter. There is no accessor for a chosen entry. The runtime reports what failed and leaves the decision of what to act on with you.
- Rendering order is reproducible output and nothing else. It is not causal precedence: the first entry is no more responsible than the last, and nothing ranks the failure kinds against each other.
- `len()` counts the entries. There is no `is_empty` — an aggregate exists only because at least one owner failed.
- `Display` renders the count followed by every entry, so an operator line elects no owner either.
- `LifecycleParticipant`, `LifecyclePhase`, and `LifecycleFailureKind` are closed, so a `match` over any of them is exhaustive and a new variant is a deliberate API change.

A server's own result stays flat. `ServerHandle` and `ServerHandleFuture` answer `Result<(), RuntimeError>` — `Cancelled`, `Timeout`, or the fatal error that ended the server — because that value has an owner already holding it. The aggregate names the owners no caller holds a handle for.

Which of those a server answers with is decided by commit order, not by rank. Every stop command commits its phase before it publishes anything, so `RuntimeError::Cancelled` is what a join reports whenever an accepted `.cancel()` committed before the server's terminal result did. A deadline or a settlement that committed first keeps its own result — `RuntimeError::Timeout` for a graceful drain that ran out of its one aggregate deadline — and the later command is then a no-op rather than an override. Two facts with no order between them are not weighed against each other.

`LifecycleParticipant::Exporter` is settlement-only vocabulary. Teardown settles the trace provider completed through `ShutdownOwner::EXPORTER`, whose shutdown is unbounded and hands nothing back, so no aggregate entry is ever recorded against it.

An upgrade callback is the one child a flat server result does not speak for. The upgrade owner polls the callback future and settles it within one fixed deadline. A callback still pending at that deadline is dropped, and the connection raises one WARN event, `camber.websocket.callback.cancelled`, carrying `disposition="cancelled"`. That event and that field are the whole observable form — the record behind them is private to the bridge. The server result still follows the accepted server command, so the callback disposition is an operator event rather than a second result.

What the aggregate cannot claim: Camber's deadlines bound Camber's own waiting and escalation. Cooperative cancellation cannot preempt an async task that never yields, cannot stop application code running on a blocking or OS thread, and cannot drop a callback whose poll or destructor blocks its thread. A participant Camber could not prove finished is named rather than reported as stopped.

If the user closure panics, the panic is the answer and nothing replaces it. Teardown still runs in full, the aggregate it produced is emitted as one `lifecycle failures displaced by an unwinding closure` event carrying the recorded count and the rendering of every entry, and the original payload then resumes.

## Integration Failures

`Integration` carries one typed account of a failed integration operation. The `Arc` lets the caller and the runtime's settlement hold the same error.

```rust
use camber::{Retryability, RuntimeError};

fn report(error: &RuntimeError) {
    let RuntimeError::Integration(failure) = error else { return };
    println!("{} {} failed: {}", failure.kind(), failure.operation(), failure.failure());
    if failure.retryability() == Retryability::OutcomeUnknown {
        // The write was submitted. It may have taken effect.
    }
    for item in failure.cleanup() {
        println!("unresolved record for {}: {:?}", item.domain(), item.record_id());
    }
}
```

- `kind()` is the closed `IntegrationKind`: `Nats`, `Sqs`, or `Dns01`.
- `operation()` is the closed `IntegrationOperation`: `Connect`, `Ready`, `Publish`, `Subscribe`, `Receive`, `Delete`, `Close`, `ZoneLookup`, `CreateTxt`, `DeleteTxt`, `Provision`, `CacheRead`, `CacheWrite`, or `Renew`. `Publish` covers a NATS publish and an SQS send. `Delete` is an SQS message deletion. A DNS record deletion is `DeleteTxt`.
- `failure()` is the closed `IntegrationFailure`: `InvalidConfig`, `Unavailable`, `PermissionDenied`, `Rejected`, `Busy`, `LimitExceeded`, `Timeout`, `Cancelled`, `Closed`, `OutcomeUnknown`, `CleanupIncomplete`, or `InvalidCertificate`.
- `retryability()` is `Never`, `Safe`, or `OutcomeUnknown`. `Safe` means a repeat cannot apply the operation twice: nothing was submitted, the service refused the request before it took effect, or the operation only reads. `OutcomeUnknown` means a write was submitted and its result was lost, so the write is neither safe to repeat nor proven absent.
- `instance_id()` is the runtime-local identity admission assigned. It is `None` when the failure came before admission.
- `cleanup()` lists each record a DNS cleanup left unresolved, as a `CleanupItem` with `domain()`, `record_id()`, and `failure()`. `record_id()` is `None` for a create whose outcome was lost. The list is empty outside cleanup failures.

### Report Budget and `Busy`

A runtime keeps at most 256 integration report accounts. One account holds one operation's failure, or reserves room for it. Reserved and retained accounts count together.

- Instance admission reserves one account for a construction failure or the eventual close. Each operation reserves one more before it submits anything.
- Success releases its account. An ordinary failure releases its account when the waiter receives it. A failure sent to a waiter that is gone is retained.
- A cleanup failure is retained, even after the caller receives it. A later successful renewal does not remove the records it names. Each account keeps at most 100 cleanup records.
- A failed close is retained after its instance retires. A successful close releases its account.
- Close, cancellation, and cleanup use the account they already hold. A full budget stops new work, not the settlement of accepted work.

When the budget is full, admission and new operations fail with `Busy` and `Retryability::Safe`, before any effect. A refusal is not retained. Retained failures are never evicted, so if they fill the budget, new integration work stays `Busy` until the runtime ends. At shutdown the runtime moves the retained accounts into its lifecycle failures once, before any resource shuts down. Each entry names `LifecycleParticipant::Integration { kind, id }` and carries the typed error with its cleanup records. An instance whose work the forced stop could not get back is retained as a failed close with `Timeout`.

Adapters choose these values from SDK error kinds, service error codes, and protocol status codes, never from diagnostic text. The third-party failure stays reachable through `std::error::Error::source`. `Display` and `Debug` never render it, because an SDK error can echo a token, credentials, a payload, a receipt handle, or a URL's userinfo and query. Each value also has one fixed lowercase name, which is the only form it takes in operator events and metric labels.

## Application-Supplied Variants

`Database` is never constructed by Camber — it ships no database layer. It exists so your own data access code can report through the same `RuntimeError` your handlers already return, rather than defining a second error type and converting at every `?`. Construct it yourself:

```rust
use camber::RuntimeError;

fn load_user(id: u64) -> Result<User, RuntimeError> {
    my_db::fetch(id).map_err(|e| RuntimeError::Database(e.to_string().into_boxed_str()))
}
```

Like every variant the boundary has no client-safe answer for, it reaches the router's rejection boundary as a redacted `500`.

## Handler Behavior

A handler error is not converted where it is raised. `IntoResponse` carries it to the router's rejection boundary, which answers:

- `RuntimeError::BadRequest` with `400` and the message you supplied, which you are declaring client-safe
- `RuntimeError::MalformedBody` with `400` and exactly `malformed request body`
- `RuntimeError::Multipart` with `400` and exactly `invalid multipart body`
- `RuntimeError::RequestBodyLimit` with `413` and exactly `request body too large`
- `RuntimeError::RequestBodyUnreadable` with `400` and exactly `request body could not be read`
- `RuntimeError::ScopeClosed` with `503` and `service unavailable`
- every other `RuntimeError` with `500` and exactly `internal server error`

The `500` body is fixed, and so is each parse refusal's. Your error's text and its whole source chain go to the operator log, never to the peer. The chain stops at an `IntegrationError`: the log records its redacted account and not the third-party source below it. `Integration` always answers `500` as an internal-service refusal, including from a middleware frame, and the rejection mapper never receives its source. A parser names which part of the grammar failed; that account is operator detail, so the peer reads fixed text instead.

The same boundary answers a middleware frame that fails. `use_middleware` accepts a frame resolving to `Response` or to `Result<Response, RuntimeError>`, and the failing frame is classified where it failed rather than becoming a response nothing can classify.

`ScopeClosed` is not a server fault. A spawn refused inside the shutdown window is an orderly drain, and `503` is the status a load balancer reads as "drain this instance". `500` is the one it reads as "this instance is broken". `NoRuntime` keeps its `500` because that one is a genuine misconfiguration of the server.

If you need a different status code, return a concrete `Response` — a deliberate `Response` is never reclassified, whatever its status — or configure `Router::rejection_mapper` to answer every refusal your own way.

## Typical Usage

```rust
use camber::RuntimeError;
use camber::http::{Request, Response};

async fn create_user(req: &Request) -> Result<Response, RuntimeError> {
    let input: CreateUser = req.json()?;
    save_user(input).await?;
    Response::empty(201)
}
```

## Direct WebSocket Termination

Behind the `ws` feature, `WebSocketClosed(WsCloseCause)` reports that a direct
WebSocket operation found its connection already over. It carries the
connection's one immutable cause — `PeerClosed`, `PeerDisconnected`,
`ServerShutdown`, `ServerCancelled`, `ReceiverDropped`, or `SendersDropped` —
and every sender clone and the receive owner read the same value.

It is held apart from the channel variants because the cause is what an
application acts on, and a channel result flattens six answers into one:

- `ChannelFull` means backpressure. `WsSender::try_send` and
  `try_send_binary` return it only while the connection is live and its bounded
  outbound queue is full, so retrying is meaningful.
- `ChannelClosed` means a send or receive found the other side gone, and carries
  no cause. It is what `camber::channel`, `select!`, and a join whose sender was
  dropped answer. A direct WebSocket half answers `WebSocketClosed` instead,
  which is the same event carrying the reason for it.
- `WebSocketClosed` means the connection is over. Every send after that reports
  it, including one that found the queue full.
- `NoRuntime` and `Timeout` separate the two ways `WsReceiver::recv_timeout`
  and `WsConn::recv_timeout` answer nothing: no Tokio clock to take a deadline
  from, and a deadline that expired. A timeout does not end the connection.

WebSocket waits are futures. They never report `BlockingInAsyncContext`, and
they make progress on a current-thread runtime. That variant keeps its meaning
for the blocking APIs that still use it, such as a blocking `MpscSender::send` on
a current-thread runtime.

Two refusals answer a `camber::spawn` issued from inside the callback,
and they keep their ordinary meanings. `ScopeClosed` says the callback's own
Camber runtime has stopped admitting; `NoRuntime` says the serving path never
carried one, which is every bare-Tokio connection.
Neither refusal runs its closure, so a receiver captured by that closure is
dropped and the connection ends with `ReceiverDropped`.

Adding this variant to the exhaustive `RuntimeError` enum is a breaking API
change for downstream matches. Calls through the `WsConn` facade are unaffected:
its receive methods still answer `None` for every cause, and its sends still
report a closed connection as `Io` with `BrokenPipe`.

## Choosing Variants

As a rule:

- use `InvalidArgument` for programmer-facing API misuse
- leave `DeadlineExceeded` to Camber's own deadline owners; it names the closed `DeadlineBoundary` a configured policy set, which `Timeout` cannot
- use `BadRequest` for caller-supplied HTTP input problems
- leave `MalformedBody` and `Multipart` to `req.json()` and `req.multipart()`, which raise them for you
- leave `RequestBodyLimit` and `RequestBodyUnreadable` to Camber's own body reading; they name a payload the framework stopped taking, not one your code found wrong
- use `Config` for startup configuration errors
- use `Secret` for secret source lookup failures

That keeps logs and HTTP behavior predictable.

## Streaming Body Refusals

A streaming multipart session reads its payload after the handler has started, so
its failures reach the handler as errors on `next_field`, `next_chunk`, and
`discard`. Three variants keep their provenance apart:

- `RequestBodyLimit` — the request or one field ran past the bytes admitted for
  it. Classified as `BodyLimit`, answered `413`.
- `RequestBodyUnreadable` — the incoming transport stopped delivering.
  Classified as `BodyUnreadable`, answered `400`.
- `Multipart` — the grammar, the counts, the headers, the boundary, nesting, the
  parser buffer, truncation, or an abandoned session. Answered `400`.

Each carries the framework's own account of what failed. That text is operator
detail; the peer reads the fixed sentence above.

Catching one of these does not clear it. A handler that swallows the error and
answers `200` is still answered with the refusal: the session recorded it, and
the session outranks the handler. A handler error keeps its own category whether
or not the body was read to its end. Only a handler that returns success over an
incomplete body is overridden, and it is answered with `Multipart`.

## Configured Service Deadlines

`DeadlineExceeded` carries a `DeadlineBoundary`: the closed name of the bound a
policy configured. `Timeout` says only that something ran long, which tells an
operator nothing about which value to change. The boundary is the same
vocabulary `RequestBudget`, `TransferBudget`, `ProxyPolicy`, `ServerPolicy`, and
`ResourceBudget` are written in, and it is what a crossed deadline reports
through the operator diagnostic behind a refusal.

Two request boundaries reach a served peer today:

- `RequestBodyIdle` — the request body left a longer quiet interval between data
  frames than the effective `RequestBudget` allows. Classified as `BodyTimeout`,
  answered `408`. What the peer still owed is unread, so the HTTP/1 connection
  closes and the HTTP/2 failure stays on its own stream.
- `RequestTotal` — the admitted request outlived its total before a response
  head committed. It covers body collection, handler execution, and response
  production, and it ends at the committed head: response-body time belongs to
  the selected download `TransferBudget`, not to the request. Classified as
  `RequestTimeout`, answered `408`, with the same unread-payload disposition.

One client boundary bounds more than one attempt:

- `ClientRetry` — an outbound client call with configured retries outlived its
  `retry_timeout`. The deadline is fixed at call entry and covers every
  attempt, every delay, and the final response body. The attempt in flight is
  dropped and no attempt starts after it.

## Configured Byte Maximums

`LimitExceeded` carries a `ByteBoundary`, and it is the other half of the same
idea: the closed name of the maximum a policy configured, so an operator reads
which ceiling to widen rather than that something was too big. It is what a
buffered collection reports when it refuses to keep reading.

Four boundaries are reachable today. Each is collected under the same checked
rule: a length the source declares above the maximum is refused before anything
is allocated, an undeclared payload is counted chunk by chunk, the crossing chunk
is dropped rather than retained, and nothing is read after it.

- `ClientResponse` — an outbound response declared or delivered more bytes than
  the client's response maximum admits.
  `ClientBuilder::unbounded_response` is the only way to remove the maximum.
- `ProxyBufferedResponse` — a buffered proxy route's upstream declared or
  delivered more bytes than the maximum that route froze. The peer is answered
  `502` with no upstream text in it; the boundary reaches the operator record
  instead. `ProxyPolicy::unbounded_buffered_response` is the only way to remove
  the maximum.
- `StaticFile` — a served file states, or grows to, more bytes than the maximum
  the entry point or the route froze. `http::serve_file_unbounded` and
  `Router::static_files_unbounded` are the only spellings that remove it.
- `ProfilingResponse` — a rendered CPU profile crossed the maximum
  `ServerPolicy` froze for it, with the `profiling` feature. No caller reaches
  this one directly: the built-in `/debug/pprof/cpu` route answers a redacted
  `500` and the boundary reaches the operator record instead.
  `ServerPolicy::unbounded_profiling_response` is the only way to remove the
  maximum.

`RequestBodyLimit` stays separate. It is the served side of the same rule, and
it answers a peer rather than a caller.

A request that ends because the server was cancelled, because its aggregate
shutdown deadline expired, or because the peer had already gone invokes no
rejection mapper at all. There is no refusal to shape: the work was stopped
rather than refused, and on two of those three rows there is no peer left to
read one.
