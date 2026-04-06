# Tokio/Axum to Camber

You already know Rust. You already know Tokio. Camber removes the ceremony for services that don't need fine-grained async control.

## Side-by-Side: HTTP Service

### Axum

```rust
use axum::{Router, routing::get, extract::Path, Json};
use serde::Serialize;

#[derive(Serialize)]
struct User { id: u64, name: String }

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/hello", get(hello))
        .route("/users/:id", get(get_user));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn hello() -> &'static str {
    "Hello, world!"
}

async fn get_user(Path(id): Path<u64>) -> Json<User> {
    Json(User { id, name: format!("User {id}") })
}
```

### Camber

```rust
use camber::RuntimeError;
use camber::http::{self, Request, Response, Router};

fn main() -> Result<(), RuntimeError> {
    let mut router = Router::new();
    router.get("/hello", |_req| async { Response::text(200, "Hello, world!") });
    router.get("/users/:id", |req| async move {
        match req.param("id") {
            Some(id) => Response::text(200, &format!("User {id}")),
            None => Response::text(400, "missing id"),
        }
    });
    http::serve("0.0.0.0:8080", router)
}
```

Handlers are async closures, but there's no `#[tokio::main]`, no extractor types, no Tower. Same result.

Use `http::serve(...)` by itself for the normal server case. Reach for `runtime::builder().run(...)` only when you need Camber runtime configuration around the server or want to scope other structured work alongside it.

## Concept Mapping

| Tokio/Axum | Camber | Notes |
|---|---|---|
| `#[tokio::main]` | `fn main() -> Result<(), RuntimeError>` | Camber boots Tokio internally |
| `tokio::spawn` | `camber::spawn` | Sync closure on Tokio's blocking pool. Returns `JoinHandle<T>` with `.join()` and `.cancel()` |
| `tokio::spawn` (async) | `camber::spawn_async` | Async future on Tokio runtime. Returns `AsyncJoinHandle<T>` (`.await` or `.cancel()`) |
| `tokio::sync::mpsc` | `camber::channel` | Crossbeam underneath, supports `select!` |
| `tokio::select!` | `camber::select!` | Same macro pattern, backed by crossbeam |
| `axum::Router` | `camber::http::Router` | Method chaining: `.get()`, `.post()`, `.proxy()` |
| `async fn handler` | `router.get(path, \|req\| async { ... })` | Async closures, no extractors, no `Pin<Box<dyn Future>>` ceremony |
| `axum::extract::State` | Closure capture | Move shared state into handler closures. No extractor needed |
| `axum::extract::Path` | `req.param("id")` | Returns `Option<&str>` |
| `axum::extract::Query` | `req.query("key")` | Returns `Option<&str>`. Also `req.query_all("key")` for repeated params |
| `axum::extract::Json` | `req.json::<T>()` | Returns `Result<T, RuntimeError>` |
| `axum::Json(value)` | `Response::json(200, &value)` | Status code is explicit |
| `axum::IntoResponse` | `camber::http::IntoResponse` | Implemented for `Response` and `Result<Response, RuntimeError>` |
| Tower middleware | `router.use_middleware(fn)` | Async. If you need request data after `.await`, copy out owned values before `async move` |
| `tower_http::cors` | `camber::http::cors` | `cors::allow_origins(&["..."])` or `cors::builder()` for full control |
| `tower_http::compression` | `camber::http::compression` | `compression::auto()` — gzip for text responses > 1KB |
| Tower rate limiting | `camber::http::rate_limit` | `rate_limit::per_second(100)?` or `rate_limit::builder()` with burst config |
| Tower body validation | `camber::http::validate` | `validate::json::<T>()` — rejects invalid JSON before the handler runs |
| `axum::extract::ws::WebSocket` | `router.ws(path, handler)` | Handler receives `(&Request, WsConn)`. Feature: `ws` |
| Axum SSE (via `Sse<impl Stream>`) | `router.get_sse(path, handler)` | Handler receives `(&Request, &mut SseWriter)`. Sync, long-lived |
| Axum streaming body | `router.get_stream(path, handler)` | Async handler returns `StreamResponse`. Push chunks via `StreamSender` |
| `reqwest::get(url).await` | `http::get(url).await?` | Async. Same `.await` pattern as reqwest |
| `reqwest::Client::builder()` | `http::client()` | `.retries(3).backoff(Duration).connect_timeout(Duration)` |
| `tokio::time::sleep` | `tokio::time::sleep` | Use directly in async handlers |
| `tokio::time::timeout` | `camber::timeout` | Maps `Elapsed` to `RuntimeError::Timeout` for `?` propagation |
| `tokio::time::interval` | `tokio::time::interval` | Use directly in async contexts (schedule callbacks) |
| `tokio::sync::Notify` | `tokio::sync::Notify` | Use directly — no wrapper needed |
| `tokio::select!` | `tokio::select!` | Use directly in async contexts; `camber::select!` is for sync crossbeam channels |
| `tokio::sync::mpsc` (async) | `camber::channel::mpsc` | Async `.recv()` for future composition |
| OpenTelemetry tracing layer | `camber::http::otel::tracing()` | W3C traceparent propagation, per-request spans. Feature: `otel` |
| `sqlx::Pool` / `sea_orm::Database` | same | Use directly inside Camber handlers and background tasks |
| `sqlx::query!` / `query_as!` | same | Camber does not wrap query execution |

## HTTP Client

### Axum (reqwest)

```rust
let resp = reqwest::get("https://api.example.com/items/1").await?;
let resp = reqwest::Client::new()
    .put("https://api.example.com/items/1")
    .body(payload)
    .send()
    .await?;
let resp = reqwest::Client::new()
    .delete("https://api.example.com/items/1")
    .send()
    .await?;
```

### Camber

```rust
let resp = http::get("https://api.example.com/items/1").await?;
let resp = http::put("https://api.example.com/items/1", &payload).await?;
let resp = http::delete("https://api.example.com/items/1").await?;
let resp = http::patch("https://api.example.com/items/1", &partial).await?;
```

No client builder, no `.send()`. Each method is a single async call.

## HTTP Client with Retries

### Axum (reqwest)

```rust
let client = reqwest::Client::builder()
    .connect_timeout(Duration::from_secs(5))
    .timeout(Duration::from_secs(10))
    .build()?;

// Manual retry loop
let mut attempts = 0;
loop {
    match client.get("https://api.example.com/data").send().await {
        Ok(resp) if resp.status().is_success() => break resp,
        Ok(_) | Err(_) if attempts < 3 => {
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempts))).await;
        }
        Ok(resp) => break resp,
        Err(e) => return Err(e.into()),
    }
}
```

### Camber

```rust
let client = http::client()
    .connect_timeout(Duration::from_secs(5))
    .read_timeout(Duration::from_secs(10))
    .retries(3)
    .backoff(Duration::from_millis(100));

let resp = client.get("https://api.example.com/data").await?;
```

Retries, exponential backoff with jitter, and transient error detection (429, 502-504, timeouts) are built in. The client is built lazily and cached.

## Middleware

### Axum (Tower)

```rust
use axum::middleware::{self, Next};
use axum::extract::Request;
use axum::response::Response;

async fn logger(req: Request, next: Next) -> Response {
    let start = std::time::Instant::now();
    let resp = next.run(req).await;
    println!("{:?}", start.elapsed());
    resp
}

let app = Router::new()
    .route("/hello", get(hello))
    .layer(middleware::from_fn(logger));
```

### Camber

```rust
router.use_middleware(|req, next| {
    let method = req.method().to_owned();
    let path = req.path().to_owned();
    let start = std::time::Instant::now();
    async move {
        let resp = next.call(req).await;
        tracing::info!("{} {} {}ms", method, path, start.elapsed().as_millis());
        resp
    }
});
```

If you only need request data before `.await`, reading directly from `req` is fine. If you need to log or inspect it after `.await`, move owned data into the future first as shown above.

## Extractors and State

### Axum

```rust
use axum::extract::{State, Path, Json, Query};

#[derive(Clone)]
struct AppState { db: PgPool }

async fn get_user(
    State(state): State<AppState>,
    Path(id): Path<u64>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<User> {
    let user = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(&state.db)
        .await
        .unwrap();
    Json(user)
}

let app = Router::new()
    .route("/users/:id", get(get_user))
    .with_state(AppState { db: pool });
```

### Camber

```rust
use camber::http::{Request, Response, Router};
use sqlx::PgPool;

fn main() -> Result<(), camber::RuntimeError> {
    let pool = runtime::block_on(PgPool::connect(&db_url))?;

    let mut router = Router::new();

    let db = pool.clone();
    router.get("/users/:id", move |req| async move {
        let id = match req.param("id") {
            Some(id) => id.to_owned(),
            None => return Response::text(400, "missing id"),
        };
        let rows = match sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
            .bind(id)
            .fetch_all(&db)
            .await
        {
            Ok(r) => r,
            Err(e) => return Response::text(500, &e.to_string()),
        };
        Response::text(200, &format!("{rows:?}"))
    });

    camber::http::serve("0.0.0.0:8080", router)
}
```

State is captured by the closure. No extractor types, no `State` wrapper, no `with_state()`. Use `sqlx` or your preferred ORM directly; Camber does not ship a database wrapper.

## CORS

### Axum (tower-http)

```rust
use tower_http::cors::{CorsLayer, Any};

let cors = CorsLayer::new()
    .allow_origin(["https://app.example.com".parse().unwrap()])
    .allow_methods(Any)
    .allow_headers(Any);

let app = Router::new()
    .route("/api/data", get(handler))
    .layer(cors);
```

### Camber

```rust
use camber::http::cors;

router.use_middleware(cors::allow_origins(&["https://app.example.com"]));
```

For fine-grained control:

```rust
router.use_middleware(
    cors::builder()
        .origins(&["https://app.example.com"])
        .methods(&["GET", "POST"])
        .headers(&["Content-Type", "Authorization"])
        .max_age(7200)
        .credentials()
        .build()
);
```

## Compression

### Axum (tower-http)

```rust
use tower_http::compression::CompressionLayer;

let app = Router::new()
    .route("/api/data", get(handler))
    .layer(CompressionLayer::new());
```

### Camber

```rust
use camber::http::compression;

router.use_middleware(compression::auto());
```

Negotiates `Accept-Encoding`, gzips text responses over 1KB. Binary and small responses pass through unchanged.

## Rate Limiting

### Axum (tower)

```rust
// Typically requires tower-governor or a custom Tower layer
use tower_governor::{GovernorConfigBuilder, GovernorLayer};

let config = GovernorConfigBuilder::default()
    .per_second(10)
    .burst_size(20)
    .finish()
    .unwrap();

let app = Router::new()
    .route("/api/data", get(handler))
    .layer(GovernorLayer { config });
```

### Camber

```rust
use camber::http::rate_limit;

router.use_middleware(rate_limit::per_second(10)?);
```

For burst control:

```rust
router.use_middleware(
    rate_limit::builder()
        .tokens(10)
        .interval(Duration::from_secs(1))
        .burst(20)
        .build()?
);
```

Lock-free token bucket. Returns `429` with `Retry-After` header when exhausted.

## Request Validation

### Axum

```rust
// Axum validates during extraction — invalid JSON returns 422
async fn create_user(Json(user): Json<CreateUser>) -> impl IntoResponse {
    // ...
}
```

### Camber

```rust
use camber::http::validate;

router.use_middleware(validate::json::<CreateUser>());
router.post("/users", |req| async move {
    let user: CreateUser = req.json().unwrap(); // safe — middleware already validated
    // ...
});
```

Validation runs as middleware before the handler. Invalid requests get `400` without reaching handler code.

## WebSocket

### Axum

```rust
use axum::extract::ws::{WebSocket, WebSocketUpgrade, Message};

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        if let Message::Text(text) = msg {
            socket.send(Message::Text(format!("echo: {text}"))).await.unwrap();
        }
    }
}

let app = Router::new().route("/ws", get(ws_handler));
```

### Camber

```rust
use camber::http::{Request, Router};
use camber::http::WsConn;

router.ws("/ws", |_req: &Request, mut conn: WsConn| {
    while let Some(text) = conn.recv() {
        conn.send(&format!("echo: {text}"))?;
    }
    Ok(())
});
```

Sync handler. The `WsConn` bridges async WebSocket IO to blocking `recv()`/`send()` calls. Also supports `recv_binary()` and `recv_message()` for mixed text/binary protocols.

## Server-Sent Events

### Axum

```rust
use axum::response::sse::{Event, Sse};
use futures_util::stream;

async fn sse_handler() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::repeat_with(|| {
        Ok(Event::default().data("tick"))
    })
    .throttle(Duration::from_secs(1));
    Sse::new(stream)
}

let app = Router::new().route("/events", get(sse_handler));
```

### Camber

```rust
use camber::http::{Request, SseWriter, Router};

router.get_sse("/events", |_req: &Request, sse: &mut SseWriter| {
    loop {
        sse.event("message", "tick")?;
        std::thread::sleep(Duration::from_secs(1));
    }
});
```

Sync handler with a blocking loop. The `SseWriter` sends SSE-formatted frames through an mpsc channel. Returns `Err` when the client disconnects.

## Streaming Responses

### Axum

```rust
use axum::body::Body;
use futures_util::stream;

async fn stream_handler() -> Body {
    let stream = stream::iter(vec![
        Ok::<_, Infallible>("chunk 1\n".into()),
        Ok("chunk 2\n".into()),
    ]);
    Body::from_stream(stream)
}
```

### Camber

```rust
use camber::http::{Request, Router, StreamResponse};

router.get_stream("/stream", |_req: &Request| {
    Box::pin(async {
        let (resp, sender) = StreamResponse::new(200);
        camber::spawn_async(async move {
            sender.send("chunk 1\n").await.ok();
            sender.send("chunk 2\n").await.ok();
        });
        resp.with_header("Content-Type", "text/plain")
    })
});
```

`StreamResponse::new()` returns both the response (for the handler to return) and a `StreamSender` (for pushing chunks asynchronously).
If a stream needs different backpressure tuning, use `StreamResponse::with_buffer(status, cap)`.

## Async Handlers

### Axum

```rust
async fn handler() -> String {
    let data = some_async_lib::fetch().await;
    format!("got: {data}")
}
```

### Camber

```rust
router.get("/data", |_req| async {
    let data = some_async_lib::fetch().await;
    Response::text(200, &format!("got: {data}"))
});
```

All handlers are async. The closure receives a `Request` and returns an async block. Use `.await` freely inside the block. Handlers can return `Response` or `Result<Response, RuntimeError>` for `?` propagation.

## OpenTelemetry Tracing

### Axum (tracing + opentelemetry)

```rust
use tracing_subscriber::layer::SubscriberExt;
use tracing_opentelemetry::OpenTelemetryLayer;

let tracer = opentelemetry_otlp::new_pipeline()
    .tracing()
    .with_exporter(opentelemetry_otlp::new_exporter().tonic())
    .install_batch(opentelemetry_sdk::runtime::Tokio)?;

let subscriber = tracing_subscriber::registry()
    .with(OpenTelemetryLayer::new(tracer));

tracing::subscriber::set_global_default(subscriber)?;

// Then add tracing middleware manually per route/layer
```

### Camber

```rust
use camber::http::otel;

router.use_middleware(otel::tracing());
```

Extracts W3C `traceparent` from incoming requests, generates span IDs, propagates context to outbound `http::get`/`http::post` calls automatically. Configure the OTLP exporter endpoint on the `RuntimeBuilder`.

## Database

### Axum (sqlx)

```rust
let pool = PgPoolOptions::new()
    .max_connections(10)
    .connect("postgres://localhost/mydb")
    .await?;

async fn get_user(State(pool): State<PgPool>, Path(id): Path<i64>) -> Json<User> {
    let user = sqlx::query_as("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    Json(user)
}
```

### Camber

```rust
use sqlx::PgPool;

let pool = PgPool::connect("postgres://localhost/mydb").await?;

let db = pool.clone();
router.get("/users/:id", move |req| async move {
    let id = req
        .param("id")
        .ok_or(RuntimeError::BadRequest("missing id".into()))?
        .to_owned();
    let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
        .bind(id)
        .fetch_one(&db)
        .await
        .map_err(|e| RuntimeError::Io(e.to_string().into()))?;
    Ok(Response::json(200, &user)?)
});
```

Use `sqlx` or your preferred ORM directly in Camber handlers. Camber does not wrap database drivers.

## IntoResponse

### Axum

```rust
use axum::response::IntoResponse;

async fn handler() -> impl IntoResponse {
    (StatusCode::OK, Json(data))
}

// Or return Result
async fn fallible() -> Result<Json<Data>, AppError> {
    let data = fetch()?;
    Ok(Json(data))
}
```

### Camber

```rust
// Handlers can return Response directly
router.get("/data", |req| async move {
    Response::json(200, &data)
});

// Or return Result<Response, RuntimeError> — auto-mapped via IntoResponse
router.get("/fallible", |req| async move {
    let data = fetch().await?;
    Ok(Response::json(200, &data))
});
```

`IntoResponse` maps `RuntimeError::BadRequest` to 400 and all other errors to 500. Handlers returning `Response` pass through unchanged.

## Concurrent Work in Handlers

### Axum

```rust
async fn fan_out() -> String {
    let (a, b) = tokio::join!(
        reqwest::get("https://api1.com/data"),
        reqwest::get("https://api2.com/data"),
    );
    format!("{} {}", a.unwrap().status(), b.unwrap().status())
}
```

### Camber

```rust
router.get("/fan-out", |_req| async {
    let (tx, rx) = camber::channel::bounded::<u16>(2);

    for url in ["https://api1.com/data", "https://api2.com/data"] {
        let tx = tx.clone();
        let url = url.to_string();
        camber::spawn_async(async move {
            let status = match camber::http::get(&url).await {
                Ok(r) => r.status(),
                Err(_) => 0,
            };
            let _ = tx.send(status);
        });
    }
    drop(tx);

    let results: Vec<u16> = rx.iter().collect();
    Response::text(200, &format!("{results:?}"))
});
```

## When to Stay with Camber

Most services don't outgrow Camber. It handles REST, gRPC, SSE, WebSockets, database access, and reverse proxying — all on a single port with shared middleware.

Stay with Camber when:
- Handlers do IO (database, HTTP calls, file reads) and return responses
- You need structured concurrency (spawn, channels, select)
- You want a single binary that does HTTP + gRPC + WebSocket + proxy
- Your team values simplicity over fine-grained async control

## When to Move to Axum

The trigger isn't a feature gap. Camber already handles REST, gRPC, WebSockets, SSE, buffered HTTP, and streaming proxying on one runtime without Tower. The threshold is when you need lower-level framework assembly: custom extractor ecosystems, Tower-native layering, or highly specialized async composition that is more important than Camber's direct, opinionated model.

The migration isn't a rewrite. Camber runs on Tokio internally. Move one handler at a time, one service at a time.
