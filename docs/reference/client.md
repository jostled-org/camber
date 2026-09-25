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
| `retry_timeout` | 30 seconds | The whole retry sequence, when retries are configured |

`request_timeout` replaces the former `read_timeout`. That name claimed a
read-level boundary it never had: the value has always bounded the complete
attempt. The per-read boundary is now `response_idle_timeout`.

`request_timeout`, `response_idle_timeout`, and the response maximum are one
stored `TransferBudget`. `retry_timeout` is not part of it. `response_budget`
replaces all of the budget; `request_timeout` and `response_idle_timeout` write
one field each. Call order is authoritative — the last write to a field is the
one the client uses.
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
attempt rather than the sequence. `retry_timeout` bounds the sequence.

## Retry Behavior

Retries apply to transient failures such as:

- connection errors
- timeouts
- `429`
- `502`, `503`, `504`

Backoff uses exponential delay with jitter: `base · 2^attempt`, plus a jitter
below `base`. Every step saturates, so a large attempt count or base never
wraps to a short wait.

### Server-Stated Delay

A transient response can state its own wait in `Retry-After`. Camber reads both
forms:

- **Delta-seconds** such as `120`: that many seconds.
- **HTTP-date** such as `Sun, 06 Nov 1994 08:49:37 GMT`: the time from now
  until that date. A date that is now or in the past means retry at once.

A valid value replaces the backoff for that retry. An invalid value is ignored,
and the configured backoff applies. Either delay is clipped to the retry
deadline below: a stated wait past it ends the call at the deadline with no
further attempt.

### One Deadline For The Sequence

`retry_timeout` bounds the whole sequence: every attempt, every response head,
every delay, and the final response body. It defaults to 30 seconds. Values
below one millisecond are clamped to one millisecond, and values above thirty
years are clamped to thirty years.

```rust
let client = camber::http::client()
    .retries(3)
    .retry_timeout(std::time::Duration::from_secs(10));
```

The deadline is fixed once, when the call begins. No attempt or delay resets
it. When it expires, Camber drops the attempt in flight and returns
`RuntimeError::DeadlineExceeded(DeadlineBoundary::ClientRetry)`. No attempt
starts after it. A delay that would end past it ends at it instead.

Runtime shutdown ends the sequence too. A pending attempt or delay returns
`RuntimeError::Cancelled`, and no further attempt starts. Dropping the call
future is ordinary caller cancellation: the attempt in flight is dropped and
nothing more is sent.

Each attempt keeps its own boundaries. A connect, request, or response-idle
timeout that expires first returns its own result. Before the response head
arrives, that result can still earn another attempt. A failure while Camber
collects the final response body is returned. When the deadline and another
result become ready with no order between them, either one can be the answer.

With zero retries there is no sequence. `retry_timeout` is then ignored, and
the attempt's own boundaries bound the whole call. The free functions never
retry.

Every replay decision belongs to Camber. The underlying Reqwest client is built
with its own retry policy disabled, so no second sender resends a request this
client refused to repeat.

### Which Methods Retry

`GET`, `HEAD`, and `OPTIONS` retry by default. Repeating one of them cannot
duplicate server-visible work.

`POST`, `PUT`, `PATCH`, and `DELETE` run at most one attempt unless you opt in:

```rust
let client = camber::http::client()
    .retries(3)
    .retry_unsafe_methods(true);
```

### What The Opt-In Authorizes

Repeating an unsafe method can duplicate a write, so the opt-in authorizes a
replay only where there is evidence the first attempt did no work.

| Outcome | Safe method | Unsafe method with opt-in |
| --- | --- | --- |
| `429`, `502`, `503`, `504` | retry | retry |
| Connect-stage failure | retry | retry |
| Any other transport failure | retry | return the error |
| Any other status | return the response | return the response |

A transient response is the server's own statement that it did not act on the
request, so the request is sent again. Its body is sent again with it.

A connect-stage failure means no server saw the request, so the attempt is
repeated.

Every other transport failure is ambiguous. Once a connection is established, a
failed send proves nothing: request headers or a body prefix may already have
reached the peer, and nothing on this side distinguishes a peer that ignored
them from one that acted on them. Camber returns that transport error rather
than writing twice.

Widen this only where you know the endpoint tolerates a duplicate — an
idempotency key, a conditional write, or a `PUT` of the whole resource.

## Response Access

Responses expose:

- `status()`
- `body()`, the body as text
- `body_bytes()`, the raw body
- `headers()`, every header as a name and value pair

## Trace Propagation

With the `otel` feature enabled and tracing middleware installed, outbound client calls inject the current `traceparent` header automatically.
