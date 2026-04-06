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

## Static Files

Use `router.static_files(prefix, dir)` for small static assets.

```rust
let mut router = Router::new();
router.static_files("/assets", "./public");
```

Files are fully buffered into memory before sending. This is a convenience for small assets, not a streaming file server.
