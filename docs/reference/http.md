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

fn handler(req: &Request) -> Result<Response, camber::RuntimeError> {
    let session = req.cookie("session_id");

    let opts = CookieOptions::new()
        .path("/")
        .same_site(SameSite::Strict)
        .secure()
        .http_only();

    Response::text(200, "ok")?.set_cookie_with("session", "abc123", &opts)
}
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
    match has_auth {
        true => next.call(req),
        false => Box::pin(async {
            Response::text(401, "unauthorized").expect("valid status")
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
    }
});
router.grpc(grpc);
```

Middleware acts as a gate: it sees the request headers but not the gRPC response body, which
streams directly from tonic.

### Middleware Interaction

gRPC requests go through the full middleware chain before reaching tonic. The middleware gate
constructs an owned `Request` from the hyper request only when middleware is registered —
zero overhead when there is none. Middleware can short-circuit (return 401, 403, 429) but
cannot rewrite the streaming response body.

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
