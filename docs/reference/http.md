# HTTP Reference

Camber's HTTP API centers on `Router`, `Request`, `Response`, and the `http::*` serve functions.

## Router

Create a router with `Router::new()`, then register routes by HTTP method:

```rust
use camber::http::{self, Request, Response, Router};

let mut router = Router::new();
router.get("/hello", |_req: &Request| async { Response::text(200, "ok") });
router.post("/users", create_user);
router.put("/users/:id", update_user);
router.delete("/users/:id", delete_user);
```

Supported registration methods include:

- `get`, `post`, `put`, `patch`, `delete`, `head`, `options`
- `get_stream`
- `get_sse` and `get_sse_with_budget`
- `ws` with the `ws` feature
- `proxy`, `proxy_stream`, and health-checked variants
- `static_files`, `static_files_with_limit`, and `static_files_unbounded`

### Request Body Admission

`Router::max_request_body` and `HostRouter::max_request_body` set a ceiling in
bytes. The default is eight MiB and the hard cap is 256 MiB. A ceiling only ever
narrows: a host-router ceiling contains every child, a child that configures one
can narrow it further, and a child that configures none inherits the host's.

`Router::body_admission` decides each body-consuming request — buffered routes
and streaming proxy routes alike — before Camber polls a single payload frame:

```rust
use camber::RuntimeError;
use camber::http::{BodyAdmission, BodyAdmissionContext, RequestBodyMode, Router};

let router = Router::new()
    .max_request_body(64 * 1024)
    .body_admission(|context: &BodyAdmissionContext<'_>| {
        match (context.mode(), context.declared_length()) {
            (RequestBodyMode::Buffered, Some(declared)) if declared > 4096 => {
                Err(RuntimeError::InvalidArgument("upload too large for this tenant".into()))
            }
            _ => Ok(BodyAdmission::new(4096)),
        }
    });
```

The policy is synchronous. `BodyAdmissionContext` is a borrowed view of what the
request head established: `request_id`, `method`, `raw_path`, `route`, `mode`,
`declared_length`, and `header`. `header` follows `Request::header` exactly —
case-insensitive, first value, `None` for an absent or non-UTF-8 value. Nothing
it lends out escapes the call, and it exposes no body.

`BodyAdmission::new(max_bytes)` admits under a selected maximum.
`BodyAdmission::with_permit(max_bytes, permit)` also hands Camber a value to
hold; Camber drops it exactly once on every terminal path — completion,
refusal, disconnect, or cancellation alike — at the release point the route's
mode names. A buffered route releases the permit when the request holding it is
released. A streaming proxy route releases it when the upload ends, drained or
stopped or refused, which is before the upstream answers and before the response
finishes streaming. The selected maximum can only narrow the configured
ceilings, never raise them.

Returning `Err` refuses the request as `BodyAdmission` — `503` by default, with
the error kept as a private diagnostic no mapper or peer sees. A panic is a
different category: `InternalService`, answered with the fixed redacted `500`.
Neither reads a body byte or reaches the handler, and neither re-enters the
policy.

Routes that consume no body never reach the policy: WebSocket upgrades on a
direct or proxied route, SSE, gRPC, internal routes, and routing terminals. They
acquire no permit, read no payload byte, and run no handler.

#### Route Modes

`RequestBodyMode::Buffered` is a route whose payload is read into the owned
`Request` before the handler runs. `RequestBodyMode::Streaming` is a streaming
proxy route, whose payload is forwarded upstream frame by frame. Both are
measured against the same effective limit; they differ only in where an admitted
frame goes.

#### Effective Limit Precedence

A request may retain or forward the narrowest of four numbers, in this order:

| Source | Effect |
|---|---|
| `HostRouter::max_request_body` | Contains every child |
| `Router::max_request_body` | Narrows further, or inherits the host's when unset |
| The 256 MiB hard cap | Clamps every configured ceiling |
| `BodyAdmission::max_bytes` | Narrows the request the policy just admitted |

A resolved child router uses its own policy and its own ceiling. Nothing raises
a limit.

#### Framing and Declared Length

A declaration above the effective limit is refused as `BodyLimit` before the
first payload poll, even when the peer withholds the body. Declarations are
compared at their own `u64` width, so a length no machine could hold is never
narrowed into a small admitted number. A body with no usable declaration —
chunked, or an HTTP/2 stream that stated no size — stops at the first frame that
would cross the limit; that frame is neither retained by a buffered `Request` nor
forwarded to an upstream. Framing Hyper itself refuses never reaches Camber, so
it raises no policy call, request identity, or mapped rejection.

#### Mapper and Transport Disposition

`BodyLimit`, `BodyAdmission`, `BodyUnreadable`, `BodyTimeout`, and `MalformedBody`
are answered by the mapper the same route classification selected, under the same
request identity, and each is mapped at most once.

Every refusal that leaves payload unread ends an HTTP/1 connection: the response
carries `Connection: close`, because the next request would otherwise be framed
out of bytes nobody read. The same applies when a streaming proxy's upstream
answers before the upload finishes — the upload is stopped, and the answer goes
out over a closing connection. On HTTP/2 nothing is closed but the refused
stream; the connection carries later streams normally.

## Requests

`Request` exposes string-based accessors:

- `req.method()`
- `req.path()`
- `req.param("id")`
- `req.query("key")`
- `req.query_all("tag")`
- `req.query_pairs()`
- `req.raw_query()`
- `req.header("host")`
- `req.cookie("session")`
- `req.body()`
- `req.json::<T>()`
- `req.multipart()`
- `req.request_id()` — the identity Camber minted for this request, not the `X-Request-Id` header the peer sent
- `req.on_disconnect()` — see [Observing Disconnect](#observing-disconnect)

### Query Views

`query` and `query_all` look values up by decoded key. `query_pairs` iterates every decoded pair in wire order. `raw_query` returns the query exactly as the peer sent it, without the leading `?`.

```rust
router.get("/search", |req| {
    // For "/search?q=a%2Bb&tag=x&tag=y":
    let raw = req.raw_query().unwrap_or("").to_owned();  // "q=a%2Bb&tag=x&tag=y"
    let keys = req                                        // "q,tag,tag"
        .query_pairs()
        .map(|(key, _)| key)
        .collect::<Vec<_>>()
        .join(",");
    async move { Response::text(200, &format!("{raw}|{keys}")) }
});
```

Raw and decoded answer different questions:

| Target | `raw_query()` | `query_pairs()` |
|---|---|---|
| `/items` | `None` | empty |
| `/items?` | `Some("")` | empty |
| `/items?a=1&a=2` | `Some("a=1&a=2")` | `("a", "1")`, `("a", "2")` |
| `/items?=blank&bare&x=` | `Some("=blank&bare&x=")` | `("", "blank")`, `("bare", "")`, `("x", "")` |
| `/items?a=1%262` | `Some("a=1%262")` | `("a", "1&2")` |

Pairs split before they decode, so `%26` and `%3D` cannot open a new pair or key boundary. An empty segment — from an empty query, or from a leading, consecutive, or trailing `&` — yields no pair.

Decoding is permissive and never fails:

- A valid `%HH` escape decodes one byte. Hexadecimal digits are case-insensitive.
- `+` and `%20` both decode to a space. `%2B` decodes to a literal plus.
- A malformed or incomplete escape stays literal text. The pair survives.
- Invalid UTF-8 becomes the Unicode replacement character.

Use `raw_query` when you must distinguish malformed escapes, validate UTF-8 strictly, or sign the request. It gives you the representation the URI parser accepted, and you apply your own policy to it.

An empty lookup name is absent from `query` and `query_all` even though `query_pairs` exposes blank keys. Form fields keep their own rule: `form` never sees a blank field name.

The URI owns the raw query, so `raw_query` borrows it and allocates nothing. The first call to `query`, `query_all`, or `query_pairs` decodes the whole query once into a cache the request owns. Every later call borrows from that one sequence.

### Handler Ownership Rule

Handlers receive `&Request`, but the future returned by the handler must be `Send + 'static`.

That means: if you need request data after an `.await`, copy out owned data before entering `async move`.

```rust
router.get("/users/:id", |req| {
    let user_id = req.param("id").unwrap_or("").to_owned();
    async move {
        let user = load_user(&user_id).await?;
        Response::json(200, &user)
    }
});
```

If you only need request data before `.await`, reading directly from `req` is fine.

### Observing Disconnect

`req.on_disconnect()` returns a `DisconnectSignal` for that one response. Awaiting `cancelled()` resolves exactly once, to a `DisconnectCause`:

| Cause | Meaning |
|---|---|
| `Completed` | the response body finished being produced |
| `ServerShutdown` | the server or runtime began shutting down |
| `PeerDisconnect` | the peer closed the connection, or the transport failed |
| `StreamReset` | this request ended early while its connection stayed live |

The signal is `Send + Sync + Clone`, and every clone resolves to the same cause. The first transition wins, and when more than one condition applies the causes resolve in the order above. `Completed` is set eagerly, before the drop that resolves the other three, so it outranks them; among the drop-resolved rows a shutdown that races a peer close reports `ServerShutdown`, and `StreamReset` is the residual — the connection is live, nothing is shutting down, and no completion was recorded. An HTTP/2 `RST_STREAM` is its canonical source, but any request that ends without producing its response over a live connection lands there on either protocol version, a panicking handler included.

**`PeerDisconnect` cannot tell a half-close from a full one.** Read EOF is what marks the connection terminating, and a client that sends its request and then calls `shutdown(WR)` produces exactly that EOF while it waits for the answer. A completed response wins the table, so this reaches one case: a handler still producing its response observes `PeerDisconnect` while a live client is still waiting for it. The transport offers no clean way to separate the two, so this is a limit of the signal rather than a case it discriminates.

**`Completed` means produced, not delivered.** It fires when Camber has handed the whole response body to Hyper, which is what an in-flight producer needs to know: it can release subprocesses, cursors, permits, and temp files. Hyper exposes frame production, not transport delivery, so "the last byte reached the client" is not observable and is not what this reports.

A response that hands its transport on rather than writing a body still completes at that handoff. A WebSocket upgrade resolves `Completed` when the `101` is committed — the point where the WebSocket subsystem takes over and the HTTP response is over; the upgraded peer's lifetime is the WebSocket close contract, not this signal. Building the `101` is not that point: an upgrade held short of its handoff has not resolved there, and one the server refuses never reaches the handoff at all. A refused upgrade falls back to its own response body, which is an ordinary one — `400` or `426` from handshake validation, `403` for a rejected Origin, and on an owned server `503` when the supervisor rejects the registration or `500` when it is unavailable — so it resolves `Completed` when that body is produced. A peer that abandons the handshake before the `101` resolves `PeerDisconnect`. A gRPC request resolves `Completed` where Camber hands it to tonic, which owns that response body.

**Hold the signal somewhere that outlives the handler.** Camber's per-request future is dropped when the peer goes away — that drop is the observation — so a handler awaiting its own signal is cancelled instead of woken. Clone the signal into the task that owns the resource:

```rust
router.get("/report", |req| {
    let disconnect = req.on_disconnect();
    async move {
        camber::spawn_async(async move {
            // Runs whether the response completed or the peer went away.
            let cause = disconnect.cancelled().await;
            release_report_resources(cause);
        });
        Response::text(200, "report queued")
    }
});
```

**The cleanup task needs a Camber runtime context.** `spawn_async` admits its future to the current runtime's root scope, so the connection task that calls it must carry that context. A `serve_background*` server started **inside** a Camber runtime propagates it: the supervisor is admitted to that runtime's root scope and runs under it, and each connection task it spawns captures that context and carries it in. The `serve_async*` entry points behave the same way: they capture the ambient context when the supervisor is built, and carry it into every connection task, so a `serve_async` server awaited inside a Camber runtime propagates it too. Started on a bare Tokio runtime, the supervisor is a plain `tokio::spawn` with nothing to capture, so its connection tasks run without a Camber context. The synchronous entry points are never in that position: `serve_listener` refuses without a runtime context and `serve` establishes one, so the same supervisor carries a Camber runtime into every connection task it spawns.

With no context `spawn_async` refuses with `RuntimeError::NoRuntime`, drops the cleanup future unrun, and reports the refusal only on the returned handle. The example above discards that handle, so on those paths it cleans up nothing and says nothing.

**A closed scope refuses it too.** Closure return from `runtime::run` closes root-scope admission, but it is not a shutdown request, so an owned server keeps serving until one arrives. In that window `spawn_async` refuses with `RuntimeError::ScopeClosed` and drops the cleanup future unrun, exactly as `NoRuntime` does. If requests can still arrive after the closure returns, read the handle, or own the cleanup task outside the scope.

Where a refusal is possible, hand the signal to a task the application owns — a `tokio::spawn` the application joins, or a channel into a worker it already runs — or start the server inside a Camber runtime that is still open and keep the pattern above. Awaiting the signal inside the handler is not an alternative: the guard that resolves it is held across the handler, so nothing can resolve while the handler is still running.

A request built with `Request::builder()` has no transport, so its signal never resolves.

## Responses

Construct responses explicitly:

```rust
Response::text(200, "hello")?
Response::json(200, &value)?
Response::empty(204)?
Response::bytes(200, bytes)?
```

All constructors return `Result<Response, RuntimeError>`.

`IntoResponse` is implemented for:

- `Response`
- `Result<Response, RuntimeError>`

That lets handlers return either directly. The conversion is fallible: it carries a handler error to the router's rejection boundary instead of turning it into a response on the spot. See [Error Handling](error.md#handler-behavior) for the answers that boundary gives.

## Rejections

Camber answers its own HTTP refusals through one boundary. A routing miss, a body
limit, a parser failure, a handler error, a handshake refusal, and a proxy failure all
become one typed value before they become a response. `Router::rejection_mapper` and
`HostRouter::rejection_mapper` replace the answer that value produces.

```rust
use camber::http::{Rejection, RejectionContext, Response, Router};

let router = Router::new().rejection_mapper(|rejection: &Rejection, context: &RejectionContext| {
    Response::json(
        rejection.status(),
        &serde_json::json!({
            "error": rejection.message(),
            "request_id": context.request_id().as_str(),
        }),
    )
});
```

The mapper is synchronous. It runs at failure and shutdown boundaries, where awaiting
application work would start a second cancellable lifecycle.

### Categories

`RejectionKind` names the producer that found the failure. Two categories a mapper
answers with the same status stay distinct.

| Kind | Default status | Default body | Raised by |
|---|---|---|---|
| `Routing` | `404`, `400` for an unusable `Host`, or `414` for a target nested too deep | `not found`, `invalid host header`, `URI path too deep` | No host or route claims the target, or the path nests past the segment limit the router matches |
| `MethodSelection` | `405` | `method not allowed` | A route claims the path, not the method |
| `BodyLimit` | `413` | `request body too large` | The body exceeds the effective limit, or a streaming multipart field exceeds `max_field_bytes` |
| `BodyAdmission` | `503` | `service unavailable` | A route's body-admission policy declined the work |
| `BodyUnreadable` | `400` | `request body could not be read` | The body stopped arriving for a reason that is not the limit |
| `BodyTimeout` | `408` | `request body timed out` | The body left a longer quiet interval between data frames than the effective `RequestBudget` allows |
| `RequestTimeout` | `408` | `request timed out` | The admitted request outlived its `RequestBudget` total before a response head committed |
| `MalformedBody` | `400` | `malformed request body` | `Request::json` could not parse the body, or the request framed its body under a transfer coding Camber does not undo |
| `Multipart` | `400` | `invalid multipart body` | `Request::multipart` could not parse the body, or a `Router::multipart` session hit a grammar, count, header, boundary, nesting, buffer, truncation, or abandonment failure |
| `InvalidHeader` | `500` | `internal server error` | A response cannot be put on the wire |
| `Application` | `400` | the message the handler declared safe | A handler returned `RuntimeError::BadRequest` |
| `Middleware` | `400` or `500` | the declared message, or fixed text | A middleware frame failed |
| `WebSocketHandshake` | `400`, `403`, or `426` | fixed handshake text | A handshake failed before `101` |
| `Proxy` | `502`, `503`, or `504` | `bad gateway`, `service unavailable`, `gateway timeout` | A proxied request failed before an upstream head |
| `InternalService` | `500` or `503` | `internal server error`, `service unavailable` | Camber could not complete the request |

A mid-body transport failure or a peer reset is not an admission refusal: it is
`BodyUnreadable`.

### Request deadlines

An admitted request carries two independent deadlines from the effective
`RequestBudget`, and neither is derived from the other.

- `body_idle` is the longest quiet interval allowed between request body data
  frames. Only a frame that delivered payload restarts it: trailers and empty
  frames renew nothing, so a peer cannot hold a body open with them.
- `total` is the lifetime from the admitted head to the committed response head.
  It covers middleware, body collection, handler execution, response production,
  a streaming proxy's upload and upstream head, and a streaming multipart
  session. Every route class spends it the same way: a chain that stalls holds a
  `get_stream`, `sse`, `ws`, proxied, or `multipart` request no longer than it
  holds a buffered one. It ends at the committed head — response-body time
  belongs to the selected download `TransferBudget`, not to the request. A
  WebSocket route ends its total at the successful handoff instead: a direct or
  proxied upgrade that never reaches its `101` is refused on the total like any
  other request, and the session behind a committed `101` spends none of it.

Both are pre-commit, so each may invoke the selected rejection mapper at most
once, and each leaves the peer's unread payload behind: the HTTP/1 connection
closes because nothing establishes where the next request would begin, and the
HTTP/2 failure stays on its own stream while the connection carries others.

The header boundary is not one of these. `ServerPolicy::header_timeout` bounds
Hyper's wait for a complete HTTP/1 request head, before any request exists. A
head that never arrives closes its transport and claims no request ID, no route
policy, no mapper call, and no completion record.

Route-aware body admission stays the only authority over request payload bytes
and permit lifetime. A deadline ends the read; it never re-counts bytes, never
re-classifies the route, and never releases the permit a second time. When a
byte maximum and a deadline both come ready in one scheduling turn, the byte
maximum wins, because it names the smaller bound the peer actually crossed.

### What a mapper is given

`Rejection` carries the category, the default status, the client-safe message, and the
safe default headers. It carries no diagnostic, no source error, no panic payload, no
upstream address, and no filesystem path. There is no accessor from it to the private
cause.

`RejectionContext` carries the request. Method, raw path, and request identifier are
present for every mapped request. Every other value is present exactly when its owner
established it:

| Failure stage | Route | Protocol | Content type | Subprotocol |
|---|---|---|---|---|
| Malformed or unmatched `Host` | absent | absent | absent | absent |
| URI depth or unmatched path | absent | absent | absent | absent |
| Wrong or unsupported method | present | absent | absent | absent |
| Body limit, timeout, or collection failure | present | present | absent | absent |
| Middleware, handler, or internal route | present | present | present after an accepted head | absent |
| WebSocket gate refusal | present | `WebSocket` | absent | absent |
| WebSocket refusal after negotiation | present | `WebSocket` | absent | present |

### Which mapper answers

The resolved child router's mapper wins. The `HostRouter`'s mapper answers a request no
child claimed. The built-in mapper answers when neither is configured, and adds
`X-Request-Id` to what it builds. Precedence is the same for buffered, head-only,
streaming, WebSocket, proxied, gRPC, and host-routed requests.

### What the framework keeps

A mapper owns the final non-informational status, the application headers, the content
type, and the body. Camber corrects the output the protocol owns, after the mapper
returns:

- A final `405` carries the `Allow` set of every route that claims the path, in `GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS` order. A static route and a parameterized one can both claim a concrete path; the header names what they serve between them.
- A final `426` on an unsupported WebSocket version carries `Sec-WebSocket-Version: 13`.
- A refusal that must close the connection overwrites `Connection` with `close`. `BodyLimit`, `BodyAdmission`, `BodyUnreadable`, `BodyTimeout`, and the `MalformedBody` an undecodable transfer coding raises each leave a framed request body unread, so all five close: nothing establishes where a next request on that connection would begin. A panicking admission policy closes for the same reason, under `InternalService`. Under HTTP/2 there is nothing to close for — each request owns its own stream, so an unread body cannot desynchronize the connection — and the header is illegal on that version, so it is removed.
- A `HEAD` sends no body bytes. The status and the representation headers do not change.

Mapper `Err`, a mapper panic, a `1xx` mapped status, or a mapped response Hyper cannot
build produces one fixed answer: status `500`, `Content-Type: text/plain`, body
`internal server error`, and `X-Request-Id`. The fallback never calls a mapper, so
response construction cannot recurse.

### Request identity

Camber mints a `RequestId` for every request Hyper hands it, before classification. It
is a 32-digit lowercase hexadecimal value, and it never comes from an inbound header —
a peer sending `X-Request-Id` is sending application data. The same value reaches
`Request::request_id`, `RejectionContext::request_id`, the built-in rejection headers,
and both operator events.

### What the mapper never sees

Failures outside Camber's control never reach a mapper: a request Hyper or rustls
refuses before Camber admits the head, a gRPC failure after the tonic handoff, a
WebSocket failure after `101`, and a streaming-source failure after the response head
is committed. No replacement response can be sent at those points.

### What operators see

Every mapped refusal records one `request rejected` event at error level, at the
conversion that puts the answer on the wire. Its fields are the request identifier, the
category, the sent status, the default status, the method, the raw path, the established
route, protocol, content type and subprotocol, the peer address, and the complete
private source chain. The message is fixed text, so the cause never becomes part of what
an operator filters on.

The status is the one the peer received. When a mapped response cannot be built, the
peer gets the fixed `500` fallback and that is the status this event records; a separate
`mapped rejection response could not be represented` event names the status policy had
chosen, so the displaced answer is reported rather than lost.

A refusal no peer receives is recorded too. An outer middleware or gate frame may answer
with its own response in place of the mapped one, and a request may end before its answer
is sent. Such a refusal has no sent status, so it records one `rejection response was not
sent` event instead, carrying the same identity, category, and private cause under a
`mapped_status` field naming what policy had chosen. It has no `status` field, and
`http_rejections_total` does not move for it: that counter is labelled by the status the
peer received, and the peer received the other answer.

Each request also records one `request completed` event carrying the same identifier
and the same sent status. For a replaced refusal that event, and `http_requests_total`,
report the answer the peer actually got.

With metrics enabled, `http_rejections_total` counts refusals under two labels: `kind`
and `status`. Both vocabularies are closed. A request identifier, a path, a route, a
peer address, and an error string are never labels.

## Cookies

Read cookies from requests and set them on responses:

```rust
use camber::http::{CookieOptions, Request, Response, SameSite};

router.get("/session", |req: &Request| {
    let session = req.cookie("session_id").unwrap_or("none").to_owned();
    async move {
        let opts = CookieOptions::new()
            .path("/")
            .same_site(SameSite::Strict)
            .secure()
            .http_only();

        Response::text(200, &session)?.set_cookie_with("session", "abc123", &opts)
    }
});
```

## Multipart Uploads

`req.multipart()` parses buffered `multipart/form-data` bodies into parts. Use it for uploads where full buffering is acceptable.

```rust
router.post("/upload", |req| async {
    let multipart = req.multipart()?;
    for part in multipart.parts() {
        save(part.filename(), part.data());
    }
    Response::text(200, "uploaded")?
});
```

## Streaming Multipart Uploads

`Router::multipart` registers an ordinary route whose handler is given metadata
and payload as they arrive, instead of a materialized body.

```rust
use camber::http::{Method, MultipartLimits, MultipartStream, Request, Response};

router.multipart(
    Method::Post,
    "/upload",
    MultipartLimits::default(),
    |_req: &Request, mut stream: MultipartStream| async move {
        while let Some(mut field) = stream.next_field().await? {
            let name = field.name().to_owned();
            while let Some(chunk) = field.next_chunk().await? {
                append(&name, &chunk);
            }
        }
        Response::text(200, "uploaded")
    },
);
```

`MultipartField` exposes `name()`, `filename()`, and `content_type()` as borrowed
strings, plus `next_chunk()` and `discard()`. `next_chunk` yields
`Option<Bytes>`; `camber::http::Bytes` is a re-export of the `bytes` crate's
type, so `bytes` is part of Camber's public API.

A `GET` multipart route does not answer `HEAD`. Every other route kind answers a
`HEAD` from its `GET` handler; this one reads a payload a `HEAD` request never
sends. Such a route names `GET` alone in its `Allow` value and refuses `HEAD`
with `405`.

### Limits

`MultipartLimits` is immutable and copyable. Build it from the defaults and
narrow what a route needs narrowed.

| Setting | Default | Bounds |
|---|---|---|
| `max_fields` | `128` | fields in one body |
| `max_field_bytes` | `8 MiB` | data bytes in one field |
| `max_headers_per_field` | `32` | header lines on one field |
| `max_header_bytes_per_field` | `16 KiB` | the whole header block of one field |
| `max_boundary_bytes` | `70` | the request boundary, which RFC 2046 caps at 70 |
| `max_chunk_bytes` | `64 KiB` | one delivered chunk |
| `max_parser_buffer_bytes` | `128 KiB` | parser capacity retained at once |

Every value must be greater than zero, `max_boundary_bytes` must not exceed the
protocol maximum of 70, and `max_parser_buffer_bytes` must be at least `max(2 *
max_header_bytes_per_field, max_chunk_bytes + max_boundary_bytes + 5)`. `build()`
returns `RuntimeError::InvalidArgument` otherwise.

There is no total-byte setting here. `Router::max_request_body`,
`HostRouter::max_request_body`, and the router's body-admission policy already
resolve one effective maximum before the first payload frame is polled.

### Ownership and backpressure

The framework owns the incoming body, the admitted permit, the parser buffers,
and the one source frame. The handler owns only its access to them. That split
decides the rest:

- **Backpressure is the command boundary.** Camber polls the transport only while
  a `next_field`, `next_chunk`, or `discard` call is outstanding. A handler that
  stops reading stalls the peer's socket on HTTP/1.1 and withholds flow-control
  credit on HTTP/2.
- **An escaped handle goes inert.** Moving `MultipartStream` into a task, a
  channel, or a longer-lived value cannot keep the body alive. When the handler
  returns, Camber revokes access and drives the session to a terminal state
  before it selects a response; later calls on the escaped handle fail.
- **Read to the end.** A handler that returns success while the body is
  incomplete is answered with `Multipart` instead. A handler that returns an
  error keeps its own category either way; the incomplete body changes only the
  connection disposition. `discard()` completes one field and is not an early
  exit from the request.

### Failure categories

| Terminal | Kind | Status |
|---|---|---|
| Total or per-field bytes exceeded | `BodyLimit` | `413` |
| The transport stopped delivering | `BodyUnreadable` | `400` |
| Grammar, count, header, boundary, nesting, buffer, or truncation | `Multipart` | `400` |
| The handler returned success over an incomplete body | `Multipart` | `400` |
| The handler returned an error, over a complete or an incomplete body | `Application` | `400` |

A parser or byte-limit failure outranks the handler: catching the error the read
handed you does not commit a success. An incomplete body is the weaker claim. A
handler error keeps its own category whether or not the body was read to its
end; only a handler that returns success over an incomplete body is overridden
with `Multipart`.

A refusal that left payload unread also decides the transport's disposition.
HTTP/1.1 answers with `Connection: close`, because no second request can be
framed behind bytes nothing read. HTTP/2 states no connection disposition; only
the refused stream ends, and the connection carries later streams.

Camber creates no file while parsing streaming multipart. Nothing spills to disk.

## WebSocket

With the `ws` feature, register WebSocket handlers with `router.ws(...)`.

```rust
use camber::http::{Request, Router, WsConn};
use camber::RuntimeError;

let mut router = Router::new();
router.ws("/chat", |_req: &Request, mut conn: WsConn| -> Result<(), RuntimeError> {
    while let Some(msg) = conn.recv() {
        conn.send(&format!("echo: {msg}"))?;
    }
    Ok(())
});
```

`WsConn` receives with `recv` (text), `recv_binary`, and `recv_message` (the next text or binary message as a `WsMessage`, with ping and pong frames still skipped). Each blocks until a message arrives or the peer closes, and each returns `None` on close. `recv_timeout(duration)` is the bounded form of `recv`: it returns `RuntimeError::Timeout` when no text message or close frame arrives before the deadline. There is no bounded form of `recv_binary` or `recv_message`.

Camber enforces a same-host Origin policy for browser WebSocket upgrades.
WebSocket upgrades are classified before request-body buffering, so upgrade requests do not hit
the normal request-body limit on the handshake path.

A handshake carries no payload. A request that declares one — a non-zero `Content-Length`, or any
`Transfer-Encoding` — is refused `400` instead of earning a `101`. The upgrade hands the transport
to the WebSocket bridge, which would leave those declared bytes unframed, and the `101` would then
go out marked `Connection: close` — the one reply RFC 6455 requires a conforming client to fail the
handshake over. Every `101` Camber emits carries `Connection: Upgrade`.

### Direction ownership

A direct WebSocket has one receive owner and any number of send handles.
`WsConn::sender()` clones the send capability and leaves the receive owner
alone. `WsConn::split()` gives up the facade for `(WsSender, WsReceiver)`.

```rust
use camber::http::{Request, Router, WsConn, WsReceive};
use camber::RuntimeError;

let mut router = Router::new();
router.ws("/events", |_req: &Request, conn: WsConn| -> Result<(), RuntimeError> {
    let (sender, mut receiver) = conn.split();
    let publisher = sender.clone();
    camber::spawn(move || publisher.send("welcome"));
    loop {
        match receiver.recv()? {
            WsReceive::Message(message) => sender.send(&format!("echo: {message:?}"))?,
            WsReceive::Closed(_cause) => return Ok(()),
        }
    }
});
```

`WsSender` is `Clone + Send + Sync`. `WsReceiver` is `Send`, is not `Clone`, and
takes `&mut self` on every receive, so one receive runs at a time.

Both halves keep the connection live. A callback that moves both halves into
owned application work may return without ending the connection. A callback that
drops `WsConn`, or that drops the last of its split halves, ends it.

### Send admission and backpressure

`ws_buffer_size` bounds each direction. Every sender clone enqueues through the
same outbound queue. `send` returns when the frame enters that queue, not when
its bytes reach the peer — the terminal table below decides whether an admitted
frame is written or dropped. `try_send` never waits and reports `ChannelFull`
while the connection is open. Both report `WebSocketClosed(cause)` once it is
over.

A borrowed binary send copies the slice once, at admission. Cloning a sender
copies the handle alone; it makes no zero-copy payload claim.

### Shared binary payloads

`send_binary(&[u8])` and `try_send_binary(&[u8])` take a borrow, so each
admission copies the slice into an owned buffer — the frame outlives the call
and the borrow does not. A producer that already owns immutable storage uses the
shared pair instead, which takes `Bytes` by value and copies nothing:

```rust
use camber::http::{Bytes, WsSender};

fn broadcast(encoded: Vec<u8>, recipients: &[WsSender]) -> Result<(), camber::RuntimeError> {
    let payload = Bytes::from(encoded);
    for recipient in recipients {
        recipient.send_shared_binary(payload.clone())?;
    }
    Ok(())
}
```

Cloning a `Bytes` changes a reference count. One payload sent to a hundred
recipients is one allocation, not a hundred copies, and dropping the producer's
own handle after admission leaves every queued clone valid.

| Operation | Input | Payload cost | Waits |
|---|---|---|---|
| `send_binary` | `&[u8]` | one copy per admission | yes |
| `try_send_binary` | `&[u8]` | one copy per admission | no |
| `send_shared_binary` | `Bytes` | none — the handle moves | yes |
| `try_send_shared_binary` | `Bytes` | none — the handle moves | no |

All four report the same results: success means the frame entered this
connection's bounded queue, `ChannelFull` means a live queue with no free slot,
and `WebSocketClosed(cause)` means the connection is over. A refused shared send
drops only the handle it was given; every other clone of that payload is
untouched.

The admitted handle belongs to the connection until its terminal cause decides
what to do with it. A cause that drains writes the frame; a cause that cancels
drops it. Either way every queued, pending, and framed handle is released when
that connection's bridge completes. `WsConn` gains no second spelling of this:
a facade caller takes its own sender with `WsConn::sender()`.

Camber makes no claim about copies the transport or the operating system makes
while writing each recipient's socket. The guarantee is that Camber creates no
new payload-sized buffer per recipient.

`send`, `recv`, and `recv_timeout` block. Where they block depends on the
caller's runtime:

| Caller | Behavior |
|---|---|
| No Tokio runtime | waits on the caller's own thread |
| Multi-thread Tokio | waits through `block_in_place`, so another worker runs |
| Current-thread Tokio | returns `BlockingInAsyncContext` before any wait |

`recv_timeout` also returns `NoRuntime` when no Tokio clock exists, and `Timeout`
when its deadline expires.

### Terminal causes

A connection has one cause. It is set once, and every sender clone and the
receiver read that same cause. When several terminal events are ready in one
turn, the highest of these wins: `ServerCancelled`, `ServerShutdown`,
`PeerClosed`, `PeerDisconnected`, `ReceiverDropped`, `SendersDropped`. A cause
fixed in an earlier turn stays authoritative, so a shutdown deadline that
escalates cannot rewrite `ServerShutdown` as `ServerCancelled`.

The cause fixes what happens to the frames each queue still holds:

| Cause | Admitted outbound frames | Queued inbound messages | Protocol close |
|---|---|---|---|
| `ServerCancelled` | cancel | discard | none |
| `ServerShutdown` | drain within the server deadline | deliver, then the cause | send close, await peer within the deadline |
| `PeerClosed` | cancel | deliver, then the cause | echo the peer's close |
| `PeerDisconnected` | cancel | deliver, then the cause | impossible |
| `ReceiverDropped` | cancel | discard | attempt a normal close |
| `SendersDropped` | drain | deliver, then the cause | send a normal close without awaiting the peer |

Every cause closes send admission at once. A send already waiting for capacity
fails there with `WebSocketClosed`.

### Callback runtime context

The callback runs on the blocking pool. What it may admit through
`camber::spawn` follows the server that is serving it:

| Serving path | `camber::spawn` in the callback |
|---|---|
| Owned server started inside a Camber runtime | admitted to that runtime's root scope, or refused with `ScopeClosed` once admission has closed |
| Owned server started on bare Tokio | refused with `NoRuntime` |
| Synchronous serving (`serve`, `serve_listener`) | admitted to the runtime the terminal call captured, which `serve` establishes when none is ambient |

A refused spawn never runs its closure, so a receiver captured by that closure is
dropped and the connection ends with `ReceiverDropped`.

The callback itself is not a root-scope child. The child it admits is, and
runtime completion waits for that child. Server completion is separate: it owns
the bridge, its two directional pumps, the transport, and the connection permit,
and it makes no claim about the callback or about application-owned work.

## Server-Sent Events

Use `router.get_sse(...)` for long-lived event streams:

```rust
use camber::http::{Request, Router, SseWriter};
use camber::RuntimeError;

let mut router = Router::new();
router.get_sse("/events", |_req: &Request, sse: &mut SseWriter| -> Result<(), RuntimeError> {
    sse.event("update", r#"{"status":"ok"}"#)?;
    Ok(())
});
```

SSE routes are also classified before request-body buffering. They keep the same handler API, but
the framework does not collect request bodies for routes that never use them.

### Bounding an event stream

`get_sse` states `TransferBudget::unbounded()` for the feed it registers: an event
stream has no payload maximum and no lifetime of its own, and its memory stays
bounded by the configured event-channel depth. Under a router, host, server, or
runtime download policy it inherits that policy rather than widening it.

`get_sse_with_budget(path, budget, handler)` is how one feed names its own payload
maximum, quiet interval between events, and lifetime. The budget is applied under
whatever the outer layers already selected: an unbounded dimension here inherits
the outer bound, and two finite values resolve to the smaller. The bounds are
enforced on the event body, after the `200` has committed, so a crossing ends the
stream rather than replacing its status.

For untrusted or long-lived consumers, set a finite download policy on the router
or the server rather than leaving every feed unbounded.

## Streaming Responses

Use `router.get_stream(...)` for chunked async responses.

`StreamResponse::new(status)` uses the default stream buffer. Use
`StreamResponse::with_buffer(status, cap)` when you need explicit channel depth control.

`StreamResponse::with_budget(status, cap, budget)` adds the transfer policy this
response's body runs under: a payload maximum, a quiet interval between frames,
and a lifetime. `new` and `with_buffer` both state `TransferBudget::unbounded()`,
which inherits the router, host, server, or runtime download policy rather than
widening it — an unbounded dimension at the response is not an unbounded
dimension at the service.

The bounds are enforced on the body, after the head has committed. A crossing
therefore ends the response rather than replacing its status: the frame that would
cross the payload maximum is never written, HTTP/1 closes the connection whose
framing cannot continue, and HTTP/2 resets the one affected stream. The route's
rejection mapper is not called — there is no response left to map.

Upload and download are accounted separately. A streaming upload's bytes, quiet
interval, and lifetime are its own, and route-aware body admission remains the
only authority over request payload bytes; a transfer policy adds time to that
path and never a second byte accountant.

Generic `StreamResponse` handlers remain on the buffered request path because the handler receives
the public owned `Request` and may inspect its body.

## Proxying

Use `proxy(...)` for buffered reverse proxying and `proxy_stream(...)` when request and response
bodies should stay streaming end to end.

- `proxy(...)` buffers the request into Camber's public `Request` model before forwarding
- `proxy_stream(...)` preserves the incoming request body stream for the upstream call
- Middleware on `proxy_stream(...)` acts as a request gate before streaming begins

### The buffered upstream maximum

A buffered proxy route reads the whole upstream answer into this process, so it
reads it under a maximum. `proxy(...)` and `proxy_checked(...)` freeze
`ProxyPolicy`'s default of eight MiB. `proxy_with_policy(...)` and
`proxy_checked_with_policy(...)` freeze the one the policy names:

```rust
use camber::http::{ProxyPolicy, Router};

let policy = ProxyPolicy::default().buffered_response_limit(1024 * 1024)?;
let mut router = Router::new();
router.proxy_with_policy("/api", "http://backend:8080", policy);
```

The maximum is frozen with the route, so two routes to one backend keep the
maximum each of them chose. An upstream that declares more than the maximum is
refused before anything is allocated; one that declares nothing is counted
frame by frame, and the frame that would cross the maximum is dropped rather
than retained. Either way the peer is answered `502` with no part of the
upstream payload in it, and the operator record names the boundary that was
crossed.

`ProxyPolicy::unbounded_buffered_response()` removes the maximum, and it is the
only proxy configuration that does. An upstream that then answers with an
unbounded or hostile body is read entirely into this process's memory: use it
only for an upstream you control and trust, and prefer `proxy_stream(...)` for
large payloads.

### Distinct proxy phases

`ProxyPolicy` bounds five things that fail separately, and each keeps its own
name in the operator record:

| Phase | Bound | Peer answer |
| --- | --- | --- |
| connect | `connect_timeout` | `502`, or `504` when the deadline expired |
| request | `request_timeout`, up to a usable upstream head | `504` |
| upstream idle | `upstream_idle_timeout`, between answer body frames | `504` before the head commits |
| upload | `upload_budget`, narrowed by route body admission | the route's own body refusal |
| download | `download_budget`, or `buffered_response_limit` | `502` buffered; post-commit the transport ends |

A failure before the response head commits is mapped once through the route's
rejection policy. A failure after it never rewrites the committed status: HTTP/1
closes the connection and HTTP/2 resets that one stream. The peer is told only
the safe sentence; the upstream's own account stays in the operator record.

`proxy_stream_with_policy(...)` and `proxy_checked_stream_with_policy(...)`
register a streaming route under a named policy; `proxy_stream(...)` and
`proxy_checked_stream(...)` take the documented defaults. Route-aware body
admission stays the request payload's byte authority — an `upload_budget`
maximum narrows what the route admits and never widens it.

Each registration freezes the client it forwards through, so a route's connect
deadline is the route's. Two routes registered under equal policies on one
router share that client; two routes under different policies never do, and no
router shares one with another.

One owner is process-wide, and only one: `proxy_forward(...)` takes a backend
and a prefix and no policy, so the documented defaults are the only bounds it
can carry. Every `proxy_forward` call in a process shares that default-policy
client and its connection pool. Register a route to name your own bounds.

## Host Routing

Use `HostRouter` to dispatch by `Host` header:

```rust
use camber::http::{self, HostRouter, Router};

let mut api = Router::new();
let mut web = Router::new();

let mut hosts = HostRouter::new();
hosts.add("api.example.com", api);
hosts.add("www.example.com", web);

let listener = camber::net::listen("0.0.0.0:8080")?;
http::serve_hosts(listener, hosts)?;
```

## Background Server Lifecycle

The four `serve_background*` functions return `Result<ServerHandle, RuntimeError>`.
The refusal is synchronous — a server that never started has no owner to hand
back — and `ServerHandle` is an armed owner of the server lifecycle. Use the
owner operations according to the transition you need:

```rust
let handle = http::serve_background(listener, router)?;

// Stop admission gracefully, then retain a concrete completion proof.
let completion: camber::http::ServerHandleFuture = handle.shutdown_and_join();
completion.await?;
```

- `shutdown(&self)` requests graceful shutdown without consuming the handle.
- `cancel(&self)` requests forced shutdown without consuming the handle.
- `join(self)` transfers control into `ServerHandleFuture` without stopping admission.
- `shutdown_and_join(self)` requests graceful shutdown, then returns that same concrete future.
- `ServerHandleFuture::shutdown(&self)` and `ServerHandleFuture::cancel(&self)` keep control available after `join`.
- Awaiting `ServerHandle` is the same concrete path as `join` and returns one flat `Result<(), RuntimeError>`.

Both owner forms are armed. `Drop` records `Abort` before releasing the control
sender. Dropping a handle or pending future therefore requests forced shutdown;
the independently running supervisor retains and joins its owned tasks. Polling
a ready `ServerHandleFuture` disarms this Drop behavior before returning the
immutable result. A control request after result completion is a no-op.

A successful join proves that each owned accepted transport, connection permit,
and registered WebSocket bridge has completed. It is safe to release resources
whose lifetime is tied to those transport owners after the join returns.

The boundary is deliberately narrower than arbitrary application execution:

- Tokio cancellation cannot preempt non-yielding async code, so the grace deadline bounds escalation to forced cancellation rather than execution time.
- Join is not proof that a non-cooperative blocking callback has returned.
- Join is not proof that a callback has released its callback-held `Request`, handler captures, or callback-side `WsConn`.
- Join does not extend runtime teardown or restore a signal watcher after that watcher is gone.

Graceful shutdown lets Hyper finish an in-flight HTTP/1 response, closes
keep-alive progression, and sends HTTP/2 GOAWAY before draining accepted
streams. Forced cancellation may close transports without graceful protocol
completion, but the returned result still waits for cooperatively abortable
owned transport tasks to be joined.

## gRPC

With the `grpc` feature, register tonic-generated services via `GrpcRouter`:

```rust
use camber::http::{GrpcRouter, Router};

let greeter = greeter_service::serve(MyGreeter);
let grpc = GrpcRouter::new().add_service(greeter);

let mut router = Router::new();
router.grpc(grpc);
```

Any tonic service that implements `NamedService` works with `add_service`. Camber dispatches
requests with `content-type: application/grpc` by matching the URI path prefix against
registered service names.

### Reflection

Register tonic's reflection service alongside your application services:

```rust
let greeter = greeter_service::serve(MyGreeter);

let reflection = tonic_reflection::server::Builder::configure()
    .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
    .build_v1()
    .unwrap();

let grpc = GrpcRouter::new()
    .add_service(greeter)
    .add_service(reflection);
```

Include the file descriptor set in your proto module:

```rust
mod proto {
    tonic::include_proto!("greeter");

    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("greeter_descriptor");
}
```

### Health Checks

Register tonic's health service for gRPC health checking protocol support:

```rust
let (health_reporter, health_service) = tonic_health::server::health_reporter();
health_reporter
    .set_service_status("greeter.Greeter", tonic_health::ServingStatus::Serving)
    .await;

let grpc = GrpcRouter::new()
    .add_service(greeter)
    .add_service(health_service);
```

### Auth via Camber Middleware

Camber middleware runs before gRPC dispatch. Use it for auth instead of tonic interceptors:

```rust
router.use_middleware(|req, next| {
    let has_auth = req
        .headers()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
    async move {
        match has_auth {
            true => Ok(next.call(req).await),
            false => Response::text(401, "unauthorized"),
        }
    }
});
router.grpc(grpc);
```

Middleware acts as a gate: it sees the request headers but not the gRPC response body, which
streams directly from tonic.

### Middleware Interaction

gRPC requests pass through the middleware gate before reaching tonic. The gate constructs an
owned `Request` from the hyper request only when middleware is registered — zero overhead when
there is none. This is a gate-only path: middleware can short-circuit (return 401, 403, 429)
but cannot wrap or rewrite the streaming response body, which streams directly from tonic.

### Streaming RPCs

Camber's `GrpcRouter` supports all tonic RPC types — unary, server-streaming,
client-streaming, and bidirectional. The tonic service trait handles streaming internally.
No additional Camber configuration is needed.

For server-streaming responses that push from a background task, use `tokio_stream::wrappers::ReceiverStream`
to adapt a `tokio::sync::mpsc::Receiver` into a `Stream`. The return type is the stream alias
generated by tonic for your service method:

```rust
type ServerStreamStream = ReceiverStream<Result<MyReply, tonic::Status>>;

async fn server_stream(
    &self,
    request: tonic::Request<MyRequest>,
) -> Result<tonic::Response<Self::ServerStreamStream>, tonic::Status> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    camber::spawn_async(async move {
        let _ = tx.send(Ok(MyReply { /* ... */ })).await;
    });
    Ok(tonic::Response::new(ReceiverStream::new(rx)))
}
```

### Testing gRPC Services

Use `runtime::test()` or `common::test_runtime()` with `serve_background` or the test helpers:

```rust
#[test]
fn grpc_responds() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let grpc = GrpcRouter::new().add_service(greeter_service::serve(MyGreeter));
            let mut router = Router::new();
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let reply = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(HelloRequest { name: "Test".into() }))
                    .await
            });

            assert_eq!(reply.unwrap().into_inner().message, "Hello, Test!");
            runtime::request_shutdown();
        })
        .unwrap();
}
```

Key patterns:
- `common::test_runtime()` returns a `RuntimeBuilder` with short timeouts
- `common::spawn_server(router)` binds to port 0 and returns the address
- `common::block_on(future)` bridges async into the sync `run()` closure
- Call `runtime::request_shutdown()` at the end to tear down cleanly

## Static Files

Use `router.static_files(prefix, dir)` for small static assets.

```rust
let mut router = Router::new();
router.static_files("/assets", "./public");
router.static_files_with_limit("/small", "./tiny", 64 * 1024)?;
```

Files are fully buffered into memory before sending. This is a convenience for
small assets, not a streaming file server.

Every read is bounded. `static_files` uses the eight-MiB default;
`static_files_with_limit` names its own maximum and refuses zero at
registration, so no request reaches the filesystem under a maximum nothing
accepted. A file past the maximum is refused as an internal service failure
carrying the typed `ByteBoundary::StaticFile` cause: a root holding content the
service cannot answer with is the operator's configuration, not the peer's
request. The maximum applies to actual bytes read, so a file that grew after it
was measured is still refused on the chunk that crosses.

`static_files_unbounded(prefix, dir)` is the explicit opt-out and the only
routed spelling that removes the ceiling. Every matched file is retained in
memory whatever its size, so it belongs to a root the operator controls. A root
untrusted input can write to makes the file's author the author of this
process's memory use.

### Serving one file directly

`http::serve_file`, `http::serve_file_with_limit`, and
`http::serve_file_unbounded` serve one file from a handler under the same three
choices and the same owner the routed family delegates to.

```rust
let answer = camber::http::serve_file(std::path::Path::new("./public"), "index.html").await?;
```

All three are `async` and fallible because the work they do is blocking and
unbounded. Each requires an entered Tokio runtime and returns
`RuntimeError::NoRuntime` before touching the filesystem when there is none.
Each copies its paths into owned values and runs path confinement, measurement,
and the checked read inside `spawn_blocking`, so no filesystem work ever runs on
a Tokio worker.

A caller that stops waiting — an expired request deadline, a cancelled request,
a shutdown — stops Camber waiting and nothing more. The blocking worker keeps
its own paths and its own buffer until it returns; Camber never reports it as
terminated.
