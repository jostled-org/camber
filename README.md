[![Crates.io](https://img.shields.io/crates/v/camber)](https://crates.io/crates/camber)
[![docs.rs](https://img.shields.io/docsrs/camber)](https://docs.rs/camber)
[![CI](https://github.com/jostled-org/camber/actions/workflows/ci.yml/badge.svg)](https://github.com/jostled-org/camber/actions/workflows/ci.yml)
[![Downloads](https://img.shields.io/crates/d/camber)](https://crates.io/crates/camber)
[![deps.rs](https://deps.rs/crate/camber/latest/status.svg)](https://deps.rs/crate/camber/latest)
[![License: MIT/Apache-2.0](https://img.shields.io/crates/l/camber)](LICENSE-MIT)

**Camber** is opinionated async Rust for IO-bound services on top of Tokio.

In development, publicly usable, and actively dogfooded.

Camber is a library and project tool for the large middle of Rust services that are IO-bound, not scheduler experiments.

## Install

```sh
cargo add camber
```

```sh
cargo install camber-cli
```

## Quick Start

Build HTTP services without extractors, Tower, or `#[tokio::main]`. Async handlers on a Tokio core.

```sh
cargo install camber-cli
camber new my-service --template http
cd my-service
cargo run
```

```rust
use camber::RuntimeError;
use camber::http::{self, Response, Router};

fn main() -> Result<(), RuntimeError> {
    let mut router = Router::new();
    router.get("/hello", |_req| async { Response::text(200, "Hello, world!") });
    http::serve("0.0.0.0:8080", router)
}
```

Use `http::serve(...)` by itself for the default case. Wrap it in `runtime::builder().run(...)` only when you need runtime configuration such as worker counts, shutdown timeouts, or registered resources.

`http::server(router)` returns a `ServerBuilder`, the canonical owned path: it takes a `ServerPolicy` and a TLS configuration, validates them synchronously, and freezes the router, the policy, and the serving context at the terminal `serve*` call. Every dimension has a default; the connection limit is the one that is unbounded when omitted, and a production service should set a finite limit. Shutdown is one aggregate deadline shared by the runtime and every server inside it — no nested owner restarts it — and `RuntimeBuilder::run` returns one `RuntimeError::Lifecycle` account of every participant that could not finish. See the [Runtime Reference](docs/reference/runtime.md) and [HTTP Reference](docs/reference/http.md).

## Docs

- [Vision](docs/vision.md)
- [Tokio/Axum to Camber](docs/guides/tokio-to-camber.md)
- [Go to Camber](docs/guides/go-to-camber.md)
- [Proxy Quickstart](docs/guides/proxy-quickstart.md)
- [Cross-Compilation](docs/guides/cross-compile.md)
- [Reference](docs/reference/README.md)
- [Runtime Reference](docs/reference/runtime.md)
- [HTTP Reference](docs/reference/http.md)
- [Middleware Reference](docs/reference/middleware.md)
- [HTTP Client Reference](docs/reference/client.md)
- [Tasks and Channels Reference](docs/reference/tasks-and-channels.md)
- [Error Reference](docs/reference/error.md)
- [Config Reference](docs/reference/config.md)
- [TLS Reference](docs/reference/tls.md)
- [Net Reference](docs/reference/net.md)
- [Resource Reference](docs/reference/resource.md)
- [Scheduling Reference](docs/reference/schedule.md)
- [Secret Reference](docs/reference/secret.md)
- [Signals and Shutdown Reference](docs/reference/signals.md)
- [Time Reference](docs/reference/time.md)
- [Logging Reference](docs/reference/logging.md)

If you're evaluating Camber as a library, start with [Tokio/Axum to Camber](docs/guides/tokio-to-camber.md) or the [Reference](docs/reference/README.md).

The README is the overview. `docs/reference/` and docs.rs are the exhaustive public surface.

## Reverse Proxy (Homelab / WIP)

Config-driven reverse proxy with auto-TLS and health checks. Suited for homelab and internal deployments. Not yet a production edge replacement.

```sh
cargo install camber-cli
camber serve config.toml
```

```toml
listen = "0.0.0.0:443"

[tls]
auto = true
email = "admin@example.com"

[[site]]
host = "jellyfin.example.com"
proxy = "http://192.168.1.10:8096"

[[site]]
host = "immich.example.com"
proxy = "http://192.168.1.10:2283"

[[site]]
host = "grafana.example.com"
proxy = "http://192.168.1.10:3000"
health_check = "/api/health"
```

Full setup guide: [Proxy Quickstart](docs/guides/proxy-quickstart.md)

## Middleware

```rust
use camber::http::{cors, compression, rate_limit};

router.use_middleware(cors::allow_origins(&["https://app.example.com"]));
router.use_middleware(compression::auto());
router.use_middleware(rate_limit::per_second(100)?);
```

Auth is just middleware:

```rust
use camber::http::Response;

router.use_middleware(|req, next| {
    let authorized = req.header("authorization").is_some_and(valid);
    async move {
        match authorized {
            true => Ok(next.call(req).await),
            false => Response::text(401, "unauthorized"),
        }
    }
});
```

If middleware needs request data after `.await`, copy out owned data before entering `async move`.

For normal HTTP handlers, middleware wraps the full owned `Request` and `Response`.
For gRPC and `proxy_stream`, middleware acts as a request gate before streaming begins.

## WebSocket & SSE

```rust
router.ws("/chat", |req, mut conn: WsConn| {
    while let Some(msg) = conn.recv() {
        conn.send(&format!("echo: {msg}"))?;
    }
    Ok(())
});
```

`conn.sender()` hands out a `Clone + Send + Sync` send handle without giving up
the receive owner, and `conn.split()` gives up the facade for both halves. Work
that holds both halves keeps the connection live after the callback returns.
Sends are admitted to a bounded queue rather than written to the peer, and both
halves read the same terminal cause. See
[docs/reference/http.md](docs/reference/http.md#direction-ownership).

To broadcast one binary payload, build it once and clone the handle:

```rust
let payload = Bytes::from(encoded);
for recipient in recipients {
    recipient.send_shared_binary(payload.clone())?;
}
```

`send_shared_binary` and `try_send_shared_binary` take `Bytes` by value and copy
nothing, so a hundred recipients cost one allocation. `send_binary(&[u8])` and
`try_send_binary(&[u8])` stay the borrowed-slice convenience and copy once at
admission. See
[docs/reference/http.md](docs/reference/http.md#shared-binary-payloads).

```rust
router.get_sse("/events", |_req, sse| {
    sse.event("update", r#"{"status":"ok"}"#)?;
    Ok(())
});
```

For generic chunked responses, use `StreamResponse::new()` for the default buffer or
`StreamResponse::with_buffer(status, cap)` when you need explicit backpressure tuning.

## HTTP Client

```rust
use camber::http;

let resp = http::get("https://api.example.com/data").await?;
let resp = http::post_json("https://api.example.com/items", &body).await?;
let resp = http::put("https://api.example.com/items/1", &body).await?;
let resp = http::delete("https://api.example.com/items/1").await?;
let resp = http::patch_json("https://api.example.com/items/1", &body).await?;

let resp = http::client().retries(3).get("https://flaky-api.example.com/data").await?;
```

## Cookies

```rust
let session = req.cookie("session_id");
let resp = Response::text(200, "ok")?.set_cookie("session_id", "abc123");
```

## File Uploads

```rust
router.post("/upload", |req| async {
    let multipart = req.multipart()?;
    for part in multipart.parts() {
        save(part.filename(), part.data());
    }
    Response::text(200, "uploaded")?
});
```

`req.multipart()` buffers the whole body first. For uploads too large to hold,
`Router::multipart` hands the handler one bounded chunk at a time. The peer's
upload advances only while the handler is reading, so a slow consumer stops the
socket instead of filling memory.

```rust
use camber::http::{Method, MultipartLimits, MultipartStream, Request, Response};

router.multipart(
    Method::Post,
    "/stream",
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

## Database

```rust
use sqlx::PgPool;

let pool = PgPool::connect("postgres://localhost/mydb").await?;

// In a handler:
let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
    .bind(id)
    .fetch_one(&pool)
    .await?;
```

Camber does not wrap database drivers. Use `sqlx` or your preferred ORM directly inside handlers and background tasks.

## Observability

```rust
use camber::http::otel;
use camber::circuit_breaker;

router.use_middleware(otel::tracing());

let protected = circuit_breaker::wrap(pool)
    .failure_threshold(3)
    .cooldown(Duration::from_secs(30))
    .build();
```

## License

Dual-licensed under MIT and Apache 2.0.
