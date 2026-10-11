use std::fs;
use std::io::{self, Write};

pub const LLMS_TXT: &str = r#"# Camber API Reference
# A Rust runtime for IO-bound services. Async handlers on a Tokio core.

## Import Patterns

```rust
use camber::http::{self, Request, Response, Router};
use camber::{spawn, JoinHandle, RuntimeError};
use camber::channel;
use camber::runtime::RuntimeBuilder;
use camber::http::{WsConn, WsMessage, WsReceive}; // feature = "ws"
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

```rust,no_run
// All handlers are async — closure returns a future
router.get("/path", |_req: &Request| async { Response::text(200, "ok") });
```

```rust,no_run
use std::future::Future;

// Named function form: constructors already return Result.
fn handler(req: &Request) -> impl Future<Output = Result<Response, RuntimeError>> + use<> {
    let path = req.path().to_owned();
    async move { Response::text(200, &path) }
}

fn routes() -> Router {
    let mut router = Router::new();
    router.get("/path", handler);
    router
}
```

```rust,no_run
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
req.header("Authorization")      -> Option<&str>
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

```rust,no_run
// Construct the downstream future before returning the async block.
router.use_middleware(|req: &Request, next| {
    let start = std::time::Instant::now();
    let method = req.method().to_owned();
    let path = req.path().to_owned();
    let response = next.call(req);
    async move {
        let resp = response.await;
        camber::tracing::info!("{} {} {}ms", method, path, start.elapsed().as_millis());
        resp
    }
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

// Any method, your own headers, typed JSON, and a per-request response maximum
let client = http::client();
let resp = client
    .request("POST", "https://api.example.com/data")?
    .sensitive_header("authorization", &token)?
    .json(&payload)?
    .response_limit(1_048_576)?
    .send().await?;

// Redirects are not followed: a 3xx is returned as the response.
// .redirect_policy(RedirectPolicy::SameOrigin { max_hops }) follows one
// only to the origin the call began at. Camber does not check the address a
// host resolves to, so send only to URLs you trust.
// total_timeout (default 30s) bounds the whole call, with or without retries.
// Retries repeat only GET, HEAD, OPTIONS, PUT, and DELETE.
// .retry_policy(RetryPolicy::Replayable) permits one request's replay.
// With the otel feature a call sends the handler's traceparent.
// .trace_context(false) on a request sends none.
// Each started call records one `http.client.completed` tracing event.
// Every call returns a ClientResponse, not a server Response
resp.status()                    -> u16
resp.header("content-type")      -> Option<&[u8]>  // first value, raw bytes
resp.headers()                   -> impl Iterator<Item = (&str, &[u8])>
resp.body_bytes()                -> &[u8]          // bytes as the peer sent them
resp.text()?                     -> &str           // strict UTF-8
resp.json::<T>()?                -> T              // ignores Content-Type
resp.error_for_status()?         -> ClientResponse // Err for 400-599
resp.into_body()                 -> Bytes

// A 4xx or 5xx is a response until error_for_status converts it.
// A handler builds its own Response from an upstream answer:
let upstream = http::get(&url).await?;
Response::text(200, upstream.text()?)
```

## Error Handling

```rust,no_run
// RuntimeError variants: Io, ChannelClosed, Timeout, Cancelled,
//   TaskPanicked, Client, Http, BadRequest, Database, Tls
// Use ? operator for propagation. BadRequest maps to 400 in handlers.
// An outbound call fails with RuntimeError::Client(ClientError).
// A handler that propagates it answers 500.

// Handler returning Result — async closure form
router.post("/numbers", |req: &Request| {
    let numbers = req.json::<Vec<u64>>();
    async move {
        let numbers = numbers?; // MalformedBody on parse failure
        Response::json(200, &numbers)
    }
});
```

## WebSocket (feature = "ws")

```rust
// The callback returns a future. Every wait is awaited.
router.ws("/chat", |_req, mut conn: WsConn| async move {
    while let Some(msg) = conn.recv().await {
        conn.send(&msg).await?;
    }
    Ok(())
});

// Read request data before `async move`. The future cannot borrow `&Request`.
router.ws("/rooms/:room", |req, conn: WsConn| {
    let room = req.param("room").unwrap_or("lobby").to_owned();
    async move {
        let (sender, mut receiver) = conn.split();
        loop {
            match receiver.recv().await? {
                WsReceive::Message(WsMessage::Text(text)) => {
                    sender.send(&format!("{room}: {text}")).await?;
                }
                WsReceive::Message(WsMessage::Binary(_)) => {}
                WsReceive::Closed(_) => return Ok(()),
            }
        }
    }
});
```

- A successful send means the frame entered the bounded outbound queue. It does
  not mean the peer received it.
- Dropping a pending send admits nothing. Dropping a pending typed receive
  consumes nothing.
- After the connection ends, `WsReceiver::recv` answers `WsReceive::Closed(cause)`
  and `WsSender` sends fail with `RuntimeError::WebSocketClosed(cause)`. The
  `WsConn` facade answers `None` and a broken pipe instead.
- `recv_timeout` fixes one deadline. Expiry returns `RuntimeError::Timeout` and
  the connection stays open.
- `WsConn::recv` skips binary messages and `recv_binary` skips text. A skipped
  message stays skipped when the receive is cancelled.
- Never block the thread in a callback. Camber cannot preempt a blocking poll.

## Other Feature-Gated APIs

```rust
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
    writeln!(io::stdout().lock(), "Wrote llms.txt")
}
