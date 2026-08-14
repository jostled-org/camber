# Error Reference

Camber uses one main error type at public API boundaries: `RuntimeError`.

Most top-level APIs return `Result<_, RuntimeError>`, so application code can use normal `?`
propagation without converting between framework-specific error types.

## Error Families

The variants cluster into a few stable buckets:

- runtime and coordination: `Io`, `Timeout`, `Cancelled`, `TaskPanicked`, channel errors
- runtime context and task lifecycle: `NoRuntime`, `ScopeClosed`, `ScopeDrainTimeout`
- request and API misuse: `BadRequest`, `InvalidArgument`
- unparseable request payloads: `MalformedBody`, `Multipart`
- request payloads Camber refused to keep reading: `RequestBodyLimit`, `RequestBodyUnreadable`
- transport and integration failures: `Http`, `Tls`, `MessageQueue`
- application-supplied: `Database`
- startup and infrastructure configuration: `Config`, `Secret`, `Dns`, `Acme`, `Schedule`

The exact enum is documented in rustdoc. The useful public rule is that Camber keeps one shared error type across these surfaces so callers do not need framework-specific conversions.

## Runtime Context and Task Lifecycle

`spawn`, `spawn_async`, and the other runtime-requiring entry points report three outcomes that are not failures of the work itself:

- `NoRuntime` — the call was made with no runtime context established. Nothing was spawned. A handle returned before a runtime exists yields this on `join`/`.await`.
- `ScopeClosed` — a runtime exists, but its root scope has already closed to admission: the `run` closure returned, or shutdown was requested. Nothing was spawned. This is the defined disposition of a spawn inside the shutdown window, not a fault. In a handler it maps to `503`, not `500` — see [Handler Behavior](#handler-behavior).
- `ScopeDrainTimeout(n)` — returned by the runtime entry point itself: `runtime::run`, `RuntimeBuilder::run`, `runtime::test`, or `#[camber::test]`. The graceful drain expired with `n` children that had not exited on their own. `n` counts children that failed to exit cooperatively, not children still running when the entry point returned.

`NoRuntime` and `ScopeClosed` are distinct so a caller can tell "no runtime at all" from "too late".

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

The `500` body is fixed, and so is each parse refusal's. Your error's text and its whole source chain go to the operator log, never to the peer. A parser names which part of the grammar failed; that account is operator detail, so the peer reads fixed text instead.

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

## Choosing Variants

As a rule:

- use `InvalidArgument` for programmer-facing API misuse
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
