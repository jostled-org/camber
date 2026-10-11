# HTTP Client Reference

Camber ships an async outbound HTTP client with:

- one-shot free functions
- a reusable `Client` for retries and timeouts
- a general request description for any method, headers, and a per-request
  response maximum

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

## Reusable Client

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

`http::client()` returns a `Client` that is ready to send. Each configuration
method consumes it and returns the changed client. There is no build step.

`Client` exposes the same request methods as the free functions. Requests
through one client share its connection pool.

## Describing A Request

`Client::request` describes one request with any method. Nothing is sent until
`send`.

```rust
use camber::http;

let client = http::client();
let response = client
    .request("POST", "https://provider.example/v1/answers")?
    .sensitive_header("authorization", &authorization)?
    .header("x-provider-version", "2026-10-01")?
    .json(&payload)?
    .response_limit(1_048_576)?
    .send()
    .await?
    .error_for_status()?;
let answer: ProviderAnswer = response.json()?;
```

| Method | Returns | Contract |
| --- | --- | --- |
| `header(name, value)` | `Result<ClientRequest, RuntimeError>` | Set an application header and replace any value the name holds |
| `sensitive_header(name, value)` | `Result<ClientRequest, RuntimeError>` | The same, and mark the name sensitive |
| `body(bytes)` | `ClientRequest` | Replace the body; `Content-Type` is not set or removed |
| `json(&value)` | `Result<ClientRequest, RuntimeError>` | Serialize once, replace the body, and set `Content-Type: application/json` |
| `response_limit(max_bytes)` | `Result<ClientRequest, RuntimeError>` | Replace this request's response maximum |
| `deadline(instant)` | `ClientRequest` | Set an absolute `std::time::Instant` ceiling for the operation; it cannot extend `total_timeout` |
| `redirect_policy(policy)` | `ClientRequest` | Replace this request's [redirect policy](#redirects) |
| `retry_policy(policy)` | `ClientRequest` | Replace this request's [replay permission](#which-methods-retry) |
| `retries(count)` | `ClientRequest` | Replace the most sends this request adds to its first one |
| `trace_context(enabled)` | `ClientRequest` | Send or withhold the automatic [trace context](#trace-propagation) |
| `send()` | async `Result<ClientResponse, RuntimeError>` | Send the request and collect its response |

The last write wins. `body` after `json` keeps the JSON content type, and
`header("content-type", ...)` after `json` replaces it.

A request owns its headers, its body, its response maximum, and its policies.
A setter changes that request only. It does not change the client or another request
through it.

### What Is Refused

Validation sends nothing. Each refusal is
`RuntimeError::Client(ClientError::InvalidRequest(kind))`:

| Input | `ClientInput` |
| --- | --- |
| A method that is not an HTTP token, or `CONNECT` | `Method` |
| A target that is not an absolute `http` or `https` URL with a host, or one with a user name or password | `Url` |
| A header name or value outside the HTTP grammar, including a CR or LF | `Header` |
| `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`, `Upgrade`, `TE`, `Trailer`, or `Proxy-Authorization` | `ReservedHeader` |
| A `TRACE` request with a body, refused by `send` | `Body` |
| A response maximum of zero | `ResponseLimit` |

A value that does not serialize makes `json` return `ClientError::Encode`.

Methods are case-sensitive. `"PURGE"` is sent as written, and `"get"` is not
`"GET"`. A URL fragment is removed before the request is sent.

### Sensitive Headers

`Authorization` and `Cookie` are always sensitive. `sensitive_header` marks any
other name. The mark is sticky: a later `header` call for the same name
replaces the value and keeps the mark. Camber sets the mark on the transport's
header value too.

`Debug` on a `ClientRequest` prints the method, the header names, the body
length, and the response maximum. It prints no target, header value, or body.

### Redirects

Each request has a `RedirectPolicy`. The default is `RedirectPolicy::None`: a
3xx answer is returned as the response, with its `Location` header and its
body, and nothing is sent to the place it names. The free functions always use
the default.

`RedirectPolicy::SameOrigin { max_hops }` follows a redirect only to the
origin the call began at. Set it on the client for every request, or on one
request:

```rust
use camber::http::{self, RedirectPolicy};
use std::num::NonZeroUsize;

let follow = RedirectPolicy::SameOrigin {
    max_hops: NonZeroUsize::new(3).ok_or("zero hops")?,
};
let client = http::client().redirect_policy(follow);

let resp = client.get("https://provider.example/v1/report").await?;
let once = client
    .request("GET", "https://provider.example/v1/report")?
    .redirect_policy(RedirectPolicy::None)
    .send()
    .await?;
```

The origin is the scheme, the host, and the port. The host is compared without
regard to case, and a port that is not written is the default port of the
scheme. Each `Location` is resolved against the URL of the request it answers
and is compared with the origin of the first request. Its fragment is removed.

Camber follows a 301, 302, 303, 307, or 308 answer that has one `Location`. An
answer with no `Location`, and every other 3xx status, is returned as the
response. A refused redirect ends the call with
`ClientError::Redirect(ClientRedirect)` and nothing is sent to the place it
names:

| Redirect | `ClientRedirect` |
| --- | --- |
| A repeated `Location`, one that is not a URL, or one with a user name or password | `InvalidLocation` |
| Another scheme, host, or port, including HTTPS to HTTP | `OriginChanged` |
| More redirects than `max_hops` in one call | `HopLimit` |
| A method the status does not let Camber send again | `UnsafeMethod` |

The status decides the method and the body of the next request:

| Status | Request method | Next request |
| --- | --- | --- |
| 301, 302 | `GET`, `HEAD` | The same method, body, and headers |
| 301, 302 | any other | Refused as `UnsafeMethod` |
| 303 | `HEAD` | `HEAD`, with no body and no `Content-Type` |
| 303 | any other | `GET`, with no body and no `Content-Type` |
| 307, 308 | `GET`, `HEAD`, `OPTIONS`, `PUT`, `DELETE` | The same method, body, and headers |
| 307, 308 | any other, under `RetryPolicy::Replayable` | The same method, body, and headers |
| 307, 308 | any other | Refused as `UnsafeMethod` |

`RetryPolicy::Never` stops retries. It does not stop a redirect.

A followed redirect carries every header of the request, credentials
included. Camber never removes credentials to send to another origin: it
refuses the redirect. Camber does not check the address a permitted host
resolves to, so this policy is not protection against server-side request
forgery. Give it only URLs you trust.

Each redirect spends one hop. Each retry spends one retry. A redirect does not
return a spent retry, and a retry does not return a spent hop, so one call
sends at most one request plus both allowances. A retry repeats the request
the last redirect led to. For example, a `POST` that a 303 changed to a `GET`
is not sent again when that `GET` is retried.

Every followed redirect runs under the same `total_timeout`, with its own
`request_timeout`. Camber releases a redirect answer before the next request
and does not read its body. Only the final answer is collected, under the
response maximum.

### Ambient Headers

The client sends no `Referer`, keeps no cookies, and adds no credentials. A
request carries the headers you set and the ones its target and body require.

## Deadlines And The Response Maximum

Each boundary is separate. One does not lend time to another.

| Dimension | Default | What it bounds | Owner | Expiry |
| --- | --- | --- | --- | --- |
| `total_timeout` | 30 seconds | One whole operation: every attempt, every followed redirect, every delay, and the final body | Camber | `ClientTimeout::Operation` |
| `request_timeout` | 30 seconds | One attempt, from its send through body end | Camber | `ClientTimeout::Attempt` |
| `connect_timeout` | 30 seconds | Establishing the transport | Reqwest | `ClientTimeout::Transport` |
| `response_idle_timeout` | 30 seconds | Each wait for response bytes | Reqwest | `ClientTimeout::Transport` |
| response maximum | eight MiB | Bytes one response may retain | Camber | `ResponseTooLarge` |

Each duration setter holds a value from one millisecond to thirty years. A
value outside that range is clamped, with a warning.

`request_timeout`, `response_idle_timeout`, and the response maximum are one
stored `TransferBudget`. `response_budget` replaces all of the budget;
`request_timeout` and `response_idle_timeout` write one field each. Call order
is authoritative — the last write to a field is the one the client uses.
Read the result back with `response_policy()`.

`total_timeout` is not part of the budget. No write to the budget removes it:
a client given `TransferBudget::unbounded()` has no attempt timer, no quiet
interval, and no response maximum, and its operations still end at the total.

A setter that changes a transport setting discards the cached transport. The
next request builds one from the current values.

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

The maximum counts the payload bytes that one response retains. It does not
count these:

- the response headers;
- the transport and TLS buffers;
- the chunk that crosses the maximum, which is dropped;
- spare capacity that the allocator reserves;
- the memory of a value that `json()` builds from the body.

The bytes are the encoded body. Camber does not decompress, so the maximum is
not a limit on decompressed size.

A crossing reports
`RuntimeError::Client(ClientError::ResponseTooLarge { limit })`. `limit` is the
maximum the collection was refused under, so an operator reads which ceiling to
widen.

`unbounded_response()` is the explicit opt-out. It removes the ceiling and
keeps both deadlines of the budget. A budget with no maximum, given to
`response_budget`, removes the ceiling too.

```rust
let client = camber::http::client().unbounded_response();
```

**Warning:** a peer that answers with an unbounded or hostile body is then read
entirely into this process's memory. Use it only for a peer you control and
trust. Every deadline survives the opt-out.

Retries are unaffected by any of this. The retry count, the backoff, and each
request's replay permission are unchanged. `request_timeout` bounds each
attempt, and `total_timeout` bounds the operation.

## Failures

A failed call returns `RuntimeError::Client(ClientError)`. The error is a
closed category. It holds no URL, header value, body, or transport error text,
and it has no source.

| Failure | `ClientError` |
| --- | --- |
| The client could not be constructed | `Configuration` |
| The request description was refused | `InvalidRequest(ClientInput)` |
| The request JSON did not serialize | `Encode` |
| The name did not resolve | `Dns` |
| The connection was refused or not established | `Connect` |
| TLS validation or protocol failure, with a Rustls cause | `Tls` |
| The operation outlived `total_timeout` or the request's `deadline` | `Timeout(ClientTimeout::Operation)` |
| One attempt outlived `request_timeout` | `Timeout(ClientTimeout::Attempt)` |
| A connect or response-idle timer expired | `Timeout(ClientTimeout::Transport)` |
| Any other send or read failure, including a truncated body | `Transport` |
| The response crossed its maximum | `ResponseTooLarge { limit }` |
| A redirect was refused under `RedirectPolicy::SameOrigin` | `Redirect(ClientRedirect)` |

A status is not a failure. A call that receives a `404` or a `503` returns
that response. See [Response Access](#response-access) for the explicit
status conversion and the two decode failures.

Camber owns the operation and attempt timers, so each names itself. Camber
does not report which transport timer expired, because Reqwest does not say.
Runtime shutdown returns `RuntimeError::Cancelled`. A call polled outside a
Tokio executor returns `RuntimeError::NoRuntime` and sends nothing.

A handler that returns one of these with `?` answers a fixed `500`. The peer
never receives the upstream status or any part of the outbound request. See the
[error reference](error.md#outbound-client-failures) for every category.

## Retry Behavior

The default retry count is zero. `Client::retries(n)` sets the most sends one
call may add to its first one. Only these outcomes earn a retry:

- a `429`, `502`, `503`, or `504` response
- a refused or timed-out connect
- a connection that closes or fails before the response head
- an attempt past `request_timeout` before the response head

Every other outcome is returned at once. That includes a DNS failure, a TLS
failure, every other status, a response above its maximum, and every failure
after the response head. `text`, `json`, and `error_for_status` are local and
never send.

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
and the configured backoff applies. Either delay is clipped to the operation
deadline below: a stated wait past it ends the call at the deadline with no
further attempt.

### One Deadline For The Operation

`total_timeout` bounds the whole operation: every attempt, every response
head, every delay, and the final response body. It defaults to 30 seconds. It
applies to every call, with zero retries or with more.

```rust
let client = camber::http::client()
    .retries(3)
    .total_timeout(std::time::Duration::from_secs(10));
```

The deadline is fixed once, when the call is first polled. No attempt or delay
resets it. When it expires, Camber drops the attempt in flight and returns
`RuntimeError::Client(ClientError::Timeout(ClientTimeout::Operation))`. No
attempt starts after it. A delay that would end past it ends at it instead.

A request can state an absolute deadline of its own:

```rust
let response = client
    .request("GET", url)?
    .deadline(std::time::Instant::now() + std::time::Duration::from_secs(2))
    .send()
    .await?;
```

The operation ends at the earlier of that instant and the total. A deadline
cannot extend the total. A deadline already in the past sends nothing and
returns `ClientTimeout::Operation`.

Runtime shutdown ends the operation too. A pending attempt or delay returns
`RuntimeError::Cancelled`, and no further attempt starts. Dropping the call
future is ordinary caller cancellation: the attempt in flight is dropped and
nothing more is sent. A handler that awaits a call inline is dropped when its
inbound peer disconnects, and the call is dropped with it. The client spawns
no task for a call.

Each attempt keeps its own boundaries. A connect, request, or response-idle
timeout that expires first returns its own result. Before the response head
arrives, that result can still earn another attempt. A failure while Camber
collects the final response body is returned. When the deadline and another
result become ready with no order between them, either one can be the answer.

An attempt timer that would not expire before the operation deadline is not
armed. The deadline ends that attempt, and the failure is
`ClientTimeout::Operation`.

The free functions never retry. They run under the default 30-second total.

Every replay decision belongs to Camber. The underlying Reqwest client is built
with its own retry policy disabled, so no second sender resends a request this
client refused to repeat.

### Which Methods Retry

Each request has a `RetryPolicy`. It says whether the request's method may be
sent again. The retry count says how many times.

| `RetryPolicy` | Methods sent again |
| --- | --- |
| `Never` | none |
| `Idempotent` (default) | `GET`, `HEAD`, `OPTIONS`, `PUT`, `DELETE` |
| `Replayable` | every method |

A method token is case-sensitive. `get` is an extension method, not `GET`, and
the default policy does not repeat it.

The policy belongs to one request. Set it, and the request's own retry count,
on the request description:

```rust
use camber::http::{self, RetryPolicy};

let client = http::client().retries(3);
let resp = client
    .request("POST", "https://provider.example/v1/charges")?
    .header("idempotency-key", "8f1c2d")?
    .body("amount=100")
    .retry_policy(RetryPolicy::Replayable)
    .retries(2)
    .send()
    .await?;
```

`ClientRequest::retries` replaces the client's count for that request. Another
request through the same client keeps its own policy and count. The verb
conveniences always use the default policy, so `client.post(...)` is sent
once.

### What `Replayable` Asserts

No outcome proves that the peer did no work. A `503`, a connection closed
before the response head, and a timeout all leave that unknown: the request
may have reached the origin, and the origin may have acted on it.

`Idempotent` relies on the method. A repeated `PUT` or `DELETE` leaves the
state one send leaves.

`Replayable` relies on you. Use it only where you know the endpoint tolerates
a duplicate, for example an idempotency key or a conditional write. Camber
adds no idempotency key and does not promise that the operation runs once.
The request body is sent again with each retry.

## Response Access

Every call returns a `ClientResponse`. It holds the complete body, collected
under the response maximum, and the headers as the peer sent them.

| Method | Returns | Notes |
| --- | --- | --- |
| `status()` | `u16` | Every status is a response, including 4xx and 5xx |
| `header(name)` | `Option<&[u8]>` | The first value of the name, matched without regard to case |
| `headers()` | iterator of `(&str, &[u8])` | Every entry; a repeated name keeps each value in the order sent |
| `body_bytes()` | `&[u8]` | The body exactly as the peer sent it |
| `into_body()` | `Bytes` | Takes the body without a copy |
| `text()` | `Result<&str, RuntimeError>` | Strict UTF-8 |
| `json::<T>()` | `Result<T, RuntimeError>` | Reads the body only; `Content-Type` is ignored |
| `error_for_status()` | `Result<ClientResponse, RuntimeError>` | Refuses 400–599 |

```rust
use camber::http;

let response = http::get("https://api.example.com/items/1")
    .await?
    .error_for_status()?;
let item: Item = response.json()?;
```

Header values are bytes. A value that is not text keeps its bytes and never
becomes an empty string. The order between different names is not the wire
order.

The body is never decoded for you. Camber disables Reqwest's automatic gzip,
Brotli, deflate, and Zstandard decoding, so an answer with `Content-Encoding`
returns its encoded bytes. `text()` and `json()` do not decompress.

The three conversions are local. None of them sends a request or retries:

| Conversion | Failure |
| --- | --- |
| `text()` on a body that is not UTF-8 | `ClientError::Decode(ClientDecode::Utf8)` |
| `json()` on a body that is not the requested JSON | `ClientError::Decode(ClientDecode::Json)` |
| `error_for_status()` on a 400–599 status | `ClientError::Status(status)` |

`Debug` prints the status, the header names, and the body length. It prints no
header value and no body content.

### Forwarding An Upstream Answer

A `ClientResponse` is not a server `Response` and does not convert into one. A
handler chooses the status, body, and headers it sends:

```rust
use camber::http::{self, Request, Response};

router.get("/proxy", |_req: &Request| async {
    let upstream = http::get("https://api.example.com/data").await?;
    Response::text(upstream.status(), upstream.text()?)
});
```

Use `Response::bytes(status, upstream.into_body())` to forward a body that is
not text.

A mock answer registered with `http::mock` returns the same `ClientResponse`
and obeys the same response maximum, including one set with `response_limit`.
A request is validated before a mock answers it. `mock::http_method` answers
exactly its method, and `mock::http` answers any method, including an
extension method such as `PURGE`.

## Completion Event

Each started operation records one `INFO` tracing event named
`http.client.completed`. The operation starts at the first poll of `send` or of
a verb call. The event is recorded when the call returns, or when its pending
future is dropped.

| Field | Value |
| --- | --- |
| `method` | `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `HEAD`, `OPTIONS`, or `TRACE`. Every other method is `OTHER` |
| `host` | The host of the URL the call began at, without the port |
| `status` | The status of the final response head, `unknown` for a status above 599, or `none` |
| `elapsed_ms` | Whole milliseconds from the first poll to the end |
| `attempts` | The requests given to the transport |
| `redirects` | The redirects followed |
| `outcome` | One word from the table below |

| `outcome` | The call ended with |
| --- | --- |
| `response` | A response, with any status |
| `invalid-request` | `ClientError::InvalidRequest`, refused by `send` |
| `configuration` | `ClientError::Configuration` |
| `no-runtime` | `RuntimeError::NoRuntime` |
| `dns`, `connect`, `tls`, `transport` | The `ClientError` of the same name |
| `timeout` | Any `ClientError::Timeout` |
| `redirect` | Any `ClientError::Redirect` |
| `response-too-large` | `ClientError::ResponseTooLarge` |
| `cancelled` | `RuntimeError::Cancelled`, at runtime shutdown |
| `abandoned` | A pending call that was dropped |

`status` is the head that Camber collects as the answer. A head released for a
retry or a redirect is not final. A truncated body and a crossed maximum
record the status of their head beside the failure outcome. A mock answer
records zero attempts. So does a call that ends before its first send: a
refused description, a missing executor, or an expired deadline.

These calls record no event:

- a refusal from `request` or from a request setter, including `json`;
- a `send` future that is never polled;
- `text()`, `json()`, and `error_for_status()` on a returned response.

The event holds no path, query, fragment, user name, password, header value,
body, `Location`, or transport error text. The retry and redirect `DEBUG`
events follow the same rule. The host is in events only. Camber adds no client
metric.

## Trace Propagation

With the `otel` feature and the `otel::tracing()` middleware, a call that a
handler sends carries the `traceparent` of that handler's request.

- Camber reads the context once, at the first poll of the call. Every retry
  and every followed redirect of that call sends the same value.
- `trace_context(false)` on a request adds no `traceparent`.
- A `traceparent` that you set with `header` is sent unchanged under either
  setting.
- A call outside the middleware has no context and adds no header.
- Without the `otel` feature, no call adds a `traceparent`.

```rust
let resp = client
    .request("GET", "https://partner.example/v1/status")?
    .trace_context(false)
    .send()
    .await?;
```

The verb conveniences always use the default. Use `Client::request` to
withhold the context from one call.
