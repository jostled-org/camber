# Error Reference

Camber uses one main error type at public API boundaries: `RuntimeError`.

Most top-level APIs return `Result<_, RuntimeError>`, so application code can use normal `?`
propagation without converting between framework-specific error types.

## Error Families

The variants cluster into a few stable buckets:

- runtime and coordination: `Io`, `Timeout`, `Cancelled`, `TaskPanicked`, channel errors
- request and API misuse: `BadRequest`, `InvalidArgument`
- transport and integration failures: `Http`, `Tls`, `Database`, `MessageQueue`
- startup and infrastructure configuration: `Config`, `Secret`, `Dns`, `Acme`, `Schedule`

The exact enum is documented in rustdoc. The useful public rule is that Camber keeps one shared error type across these surfaces so callers do not need framework-specific conversions.

## Handler Behavior

In HTTP handlers, `IntoResponse` maps:

- `RuntimeError::BadRequest` to `400`
- all other `RuntimeError` values to `500`

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
