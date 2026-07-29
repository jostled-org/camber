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
- `req.header("host")`
- `req.cookie("session")`
- `req.body()`
- `req.json::<T>()`
- `req.multipart()`
- `req.on_disconnect()` — see [Observing Disconnect](#observing-disconnect)

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

That lets handlers return either directly.

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
            true => next.call(req).await,
            false => Response::text(401, "unauthorized")?.into_response(),
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
