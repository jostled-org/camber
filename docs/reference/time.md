# Time Reference

Camber currently exposes one public time helper: `camber::timeout`.

## `timeout`

`timeout(duration, future).await` runs a future with a deadline and returns `RuntimeError::Timeout` if the deadline expires.

```rust
use camber::timeout;
use std::time::Duration;

let value = timeout(Duration::from_secs(1), async {
    do_work().await
}).await?;
```

This is a convenience wrapper around `tokio::time::timeout` that maps Tokio's elapsed error into Camber's main error type.

Use it when you want `?` propagation to stay inside `RuntimeError`.
