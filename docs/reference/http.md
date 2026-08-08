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
- `get_sse`
- `ws` with the `ws` feature
- `proxy`, `proxy_stream`, and health-checked variants
- `static_files`

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

**The cleanup task needs a Camber runtime context.** `spawn_async` admits its future to the current runtime's root scope, so the connection task that calls it must carry that context. A `serve_background*` server started **inside** a Camber runtime propagates it: the supervisor is admitted to that runtime's root scope and runs under it, and each connection task it spawns captures that context and carries it in. The `serve_async*` entry points behave the same way: they capture the ambient context when the supervisor is built, and carry it into every connection task, so a `serve_async` server awaited inside a Camber runtime propagates it too. Started on a bare Tokio runtime, the supervisor is a plain `tokio::spawn` with nothing to capture, so its connection tasks run without a Camber context — the same position as the synchronous `http::serve` entry, whose connection tasks are detached and carry no Camber runtime context by contract.

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
| `BodyLimit` | `413` | `request body too large` | The body exceeds the effective limit |
| `BodyUnreadable` | `400` | `request body could not be read` | The body stopped arriving for a reason that is not the limit |
| `BodyTimeout` | `408` | `request body timed out` | The body missed its collection deadline |
| `MalformedBody` | `400` | `malformed request body` | `Request::json` could not parse the body |
| `Multipart` | `400` | `invalid multipart body` | `Request::multipart` could not parse the body |
| `InvalidHeader` | `500` | `internal server error` | A response cannot be put on the wire |
| `Application` | `400` | the message the handler declared safe | A handler returned `RuntimeError::BadRequest` |
| `Middleware` | `400` or `500` | the declared message, or fixed text | A middleware frame failed |
| `WebSocketHandshake` | `400`, `403`, or `426` | fixed handshake text | A handshake failed before `101` |
| `Proxy` | `502`, `503`, or `504` | `bad gateway`, `service unavailable`, `gateway timeout` | A proxied request failed before an upstream head |
| `InternalService` | `500` or `503` | `internal server error`, `service unavailable` | Camber could not complete the request |

`BodyAdmission` completes the taxonomy. It is reserved for route-aware admission
control and has no producer today. A mid-body transport failure or a peer reset is
not an admission refusal: it is `BodyUnreadable`.

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
- A refusal that must close the connection overwrites `Connection` with `close`. `BodyLimit`, `BodyUnreadable`, and `BodyTimeout` each leave the request body unread, so all three close: nothing establishes where a next request on that connection would begin. Under HTTP/2 there is nothing to close for — each request owns its own stream, so an unread body cannot desynchronize the connection — and the header is illegal on that version, so it is removed.
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

## Streaming Responses

Use `router.get_stream(...)` for chunked async responses.

`StreamResponse::new(status)` uses the default stream buffer. Use
`StreamResponse::with_buffer(status, cap)` when you need explicit channel depth control.

Generic `StreamResponse` handlers remain on the buffered request path because the handler receives
the public owned `Request` and may inspect its body.

## Proxying

Use `proxy(...)` for buffered reverse proxying and `proxy_stream(...)` when request and response
bodies should stay streaming end to end.

- `proxy(...)` buffers the request into Camber's public `Request` model before forwarding
- `proxy_stream(...)` preserves the incoming request body stream for the upstream call
- Middleware on `proxy_stream(...)` acts as a request gate before streaming begins

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

The four `serve_background*` functions return `ServerHandle`, an armed owner of
the server lifecycle. Use the owner operations according to the transition you
need:

```rust
let handle = http::serve_background(listener, router);

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
        .keepalive_timeout(Duration::from_millis(500))
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
```

Files are fully buffered into memory before sending. This is a convenience for small assets, not a streaming file server.
