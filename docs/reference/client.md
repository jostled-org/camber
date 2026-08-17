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
    .request_timeout(Duration::from_secs(10))
    .response_idle_timeout(Duration::from_secs(2))
    .retries(3)
    .backoff(Duration::from_millis(100));

let resp = client.get("https://api.example.com/data").await?;
```

`ClientBuilder` exposes the same request methods as the free functions.

## Deadlines And The Response Maximum

Each boundary is separate. One does not lend time to another.

| Dimension | Default | What it bounds |
| --- | --- | --- |
| `connect_timeout` | 30 seconds | Establishing the transport |
| `request_timeout` | 30 seconds | One whole attempt, connect through body end |
| `response_idle_timeout` | 30 seconds | Each gap between response body reads |
| response maximum | eight MiB | Bytes one response may retain |

`request_timeout` replaces the former `read_timeout`. That name claimed a
read-level boundary it never had: the value has always bounded the complete
attempt. The per-read boundary is now `response_idle_timeout`.

The last three are one stored `TransferBudget`. `response_budget` replaces all
of it; `request_timeout` and `response_idle_timeout` write one field each. Call
order is authoritative — the last write to a field is the one the client uses.
Read the result back with `response_policy()`.

```rust
use camber::http::{self, TransferBudget};
use std::time::Duration;

let client = http::client().response_budget(TransferBudget::bounded(
    64 * 1024,
    Duration::from_secs(5),
    Duration::from_secs(20),
)?);
```

A zero maximum or deadline is refused where the budget is built, so no client
is constructed holding one. Zero never means unbounded.

## Bounded Response Collection

A response is collected under the maximum above. A peer that declares a length
larger than the maximum is refused before anything is allocated. A body whose
length is unknown is read incrementally, counted with checked addition before
each chunk is kept, and the chunk that crosses the maximum is dropped rather
than retained. Nothing is read after that. Trailers cost no payload bytes.

A crossing reports `RuntimeError::LimitExceeded(ByteBoundary::ClientResponse)`,
so an operator reads which maximum to widen.

`unbounded_response()` is the explicit opt-out and the only way to remove the
ceiling:

```rust
let client = camber::http::client().unbounded_response();
```

**Warning:** a peer that answers with an unbounded or hostile body is then read
entirely into this process's memory. Use it only for a peer you control and
trust. Both deadlines survive the opt-out.

Retries are unaffected by any of this. Retry eligibility, count, backoff, and
the unsafe-method opt-in are unchanged, and `request_timeout` bounds each
attempt rather than the sequence.

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
