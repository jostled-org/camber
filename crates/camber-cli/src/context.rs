use std::fs;
use std::io;

pub const LLMS_TXT: &str = r#"# Camber API Reference
# A Rust runtime for IO-bound services. Async handlers on a Tokio core.

## Import Patterns

```rust
use camber::http::{self, Request, Response, Router};
use camber::{spawn, JoinHandle, RuntimeError};
use camber::channel;
use camber::runtime::RuntimeBuilder;
```

## Entry Points

```rust
// Serve HTTP on address with router
http::serve("0.0.0.0:8080", router) -> Result<(), RuntimeError>

// Custom runtime configuration
RuntimeBuilder::new()
    .worker_threads(4)
    .shutdown_timeout(Duration::from_secs(30))
    .run(|| { ... }) -> Result<T, RuntimeError>

// Run closure in default runtime (non-HTTP use cases)
runtime::run(|| { ... }) -> Result<T, RuntimeError>
```

## Handler Signature

```rust
// All handlers are async — closure returns a future
router.get("/path", |req: &Request| async { Response::text(200, "ok") });

// Named function form
fn handler(req: &Request) -> impl Future<Output = Response> {
    async { Response::text(200, "ok") }
}

// Handlers can return Result for automatic error mapping
fn handler(req: &Request) -> impl Future<Output = Result<Response, RuntimeError>> {
    async { Ok(Response::text(200, "ok")) }
}

// With path params — clone/extract before async block
router.get("/users/:id", |req: &Request| {
    let id = req.param("id").unwrap_or("missing").to_owned();
    async move { Response::text(200, &id) }
});
```

## Request

```rust
req.method()                     -> &str       // "GET", "POST", etc.
req.path()                       -> &str       // URL path without query
req.param("id")                  -> Option<&str>   // Path parameter :id
req.query("page")                -> Option<&str>   // Query string value
req.query_all("tag")             -> impl Iterator   // All values for key
req.form("email")                -> Option<&str>   // Form field
req.header("Authorization")      -> via headers()
req.headers()                    -> impl Iterator<Item = (&str, &str)>
req.body()                       -> &str           // UTF-8 body
req.body_bytes()                 -> &[u8]          // Raw bytes
req.json::<T>()                  -> Result<T, RuntimeError>
```

## Response

```rust
Response::text(200, "hello")             // text/plain
Response::json(200, &value)              // application/json (impl Serialize)
Response::bytes(200, data)               // raw bytes
Response::empty(204)                     // no body
resp.with_header("X-Custom", "value")    // add header
resp.with_content_type("text/html")      // set content type
```

## Router

```rust
let mut router = Router::new();
router.get("/path", handler);                      // async handler
router.post("/path", handler);
router.put("/path", handler);
router.delete("/path", handler);
router.get("/users/:id", handler);                  // path parameters
router.static_files("/assets", "./public");         // static file serving
router.proxy("/api", "http://backend:3000");        // reverse proxy
router.use_middleware(logger);                      // async middleware
```

## Middleware

```rust
// All middleware is async — next.call(req) returns a future
router.use_middleware(|req: &Request, next: Next| async move {
    let start = std::time::Instant::now();
    let resp = next.call(req).await;
    println!("{} {} {}ms", req.method(), req.path(), start.elapsed().as_millis());
    resp
});

// First registered = outermost. Can short-circuit by not calling next.
```

## Structured Concurrency

```rust
// Spawn a background task
let handle: JoinHandle<T> = spawn(|| { ... });
let result = handle.join()?;
handle.cancel(); // request cancellation

// Channels with cancellation support
let (tx, rx) = channel::new::<String>();   // bounded, default capacity
let (tx, rx) = channel::bounded::<i32>(64); // explicit capacity
tx.send(value)?;       // Err on closed or cancelled
let val = rx.recv()?;  // blocks, respects cancellation
for val in rx.iter() { ... } // cancel-aware iterator
```

## Outbound HTTP

```rust
// All client functions are async
let resp = http::get("https://api.example.com/data").await?;
let resp = http::post("https://api.example.com/data", body).await?;
let resp = http::post_json("https://api.example.com/data", json_str).await?;

// With timeouts
let resp = http::client()
    .connect_timeout(Duration::from_secs(5))
    .get("https://api.example.com/data").await?;
```

## Error Handling

```rust
// RuntimeError variants: Io, ChannelClosed, Timeout, Cancelled,
//   TaskPanicked, Http, BadRequest, Database, Tls
// Use ? operator for propagation. BadRequest maps to 400 in handlers.

// Handler returning Result — async closure form
router.get("/user", |req: &Request| async {
    let user: User = req.json()?;  // BadRequest on parse failure
    Ok(Response::json(200, &user))
});
```

## Feature-Gated APIs

```rust
// WebSocket (feature = "ws")
router.ws("/chat", |req, mut conn: WsConn| {
    while let Some(msg) = conn.recv() {
        conn.send(&msg)?;
    }
    Ok(())
});

// gRPC (feature = "grpc")
router.grpc(GrpcRouter::new().add_service(my_service));
```

## Avoid

- Do not call `tokio::spawn` directly. Use `camber::spawn` for cancellation support.
- Do not use `unwrap()` or `panic!()`. Return `Result` and use `?`.
- Do not use `std::thread::spawn`. Use `camber::spawn` for runtime integration.
- Do not create a Tokio runtime manually. Camber manages the runtime.
"#;

pub fn run() -> io::Result<()> {
    fs::write("llms.txt", LLMS_TXT)?;
    println!("Wrote llms.txt");
    Ok(())
}
