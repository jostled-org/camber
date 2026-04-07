# Secrets Reference

Camber's secret helpers load small secret values from sources outside a config file.

## `SecretRef`

`SecretRef` points to one of two sources: an environment variable or a file path.

## `load_secret`

Use `load_secret(&secret_ref)` to resolve the value:

```rust
use camber::secret::{SecretRef, load_secret};

let token = load_secret(&SecretRef::Env("API_TOKEN".into()))?;
```

Behavior is intentionally simple: the value is loaded once, surrounding whitespace is trimmed, and source failures are reported as `RuntimeError::Secret`.

## Intended Use

This helper is deliberately small.

- It is for startup-time secret loading.
- It is not a secret manager abstraction.
- It does not cache, refresh, or watch secrets.
