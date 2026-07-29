# Error Reference

Camber uses one main error type at public API boundaries: `RuntimeError`.

Most top-level APIs return `Result<_, RuntimeError>`, so application code can use normal `?`
propagation without converting between framework-specific error types.

## Error Families

The variants cluster into a few stable buckets:

- runtime and coordination: `Io`, `Timeout`, `Cancelled`, `TaskPanicked`, channel errors
- runtime context and task lifecycle: `NoRuntime`, `ScopeClosed`, `ScopeDrainTimeout`
- request and API misuse: `BadRequest`, `InvalidArgument`
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

Like every variant other than `BadRequest` and `ScopeClosed`, it maps to `500` through `IntoResponse`.

## Handler Behavior

In HTTP handlers, `IntoResponse` maps:

- `RuntimeError::BadRequest` to `400`
- `RuntimeError::ScopeClosed` to `503`
- all other `RuntimeError` values to `500`

`ScopeClosed` is not a server fault. A spawn refused inside the shutdown window is an orderly drain, and `503` is the status a load balancer reads as "drain this instance". `500` is the one it reads as "this instance is broken". `NoRuntime` keeps its `500` because that one is a genuine misconfiguration of the server.

If you need a different status code, return a concrete `Response` instead of relying on automatic mapping.

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
- use `Config` for startup configuration errors
- use `Secret` for secret source lookup failures

That keeps logs and HTTP behavior predictable.
