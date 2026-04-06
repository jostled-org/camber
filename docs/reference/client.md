# HTTP Client Reference

Camber ships an async outbound HTTP client with:

- one-shot free functions
- a reusable `ClientBuilder` for retries and timeouts

## One-Shot Requests

```rust
use camber::http;

let resp = http::get("https://api.example.com/data").await?;
let resp = http::post("https://api.example.com/items", &payload).await?;
let resp = http::post_json("https://api.example.com/items", &body).await?;
let resp = http::put("https://api.example.com/items/1", &payload).await?;
let resp = http::delete("https://api.example.com/items/1").await?;
let resp = http::patch_json("https://api.example.com/items/1", &partial).await?;
```

Use these when defaults are fine.

## Reusable ClientBuilder

```rust
use camber::http;
use std::time::Duration;

let client = http::client()
    .connect_timeout(Duration::from_secs(5))
    .read_timeout(Duration::from_secs(10))
    .retries(3)
    .backoff(Duration::from_millis(100));

let resp = client.get("https://api.example.com/data").await?;
```

`ClientBuilder` exposes the same request methods as the free functions.

## Retry Behavior

Retries apply to transient failures such as:

- connection errors
- timeouts
- `429`
- `502`, `503`, `504`

Backoff uses exponential delay with jitter.

## Response Access

Responses expose:

- `status()`
- `body()`
- `header(name)`
- `headers()`

## Trace Propagation

With the `otel` feature enabled and tracing middleware installed, outbound client calls inject the current `traceparent` header automatically.
