# Go to Camber

Go concepts mapped to Camber equivalents. If you've written `net/http` services in Go, you already know the patterns — Camber uses the same model with Rust's type safety.

## HTTP Handler

### Go

```go
func main() {
    http.HandleFunc("/hello", hello)
    http.HandleFunc("/users/", getUser)
    http.ListenAndServe(":8080", nil)
}

func hello(w http.ResponseWriter, r *http.Request) {
    fmt.Fprintf(w, "Hello, world!")
}

func getUser(w http.ResponseWriter, r *http.Request) {
    id := r.PathValue("id")
    fmt.Fprintf(w, "User %s", id)
}
```

### Camber

```rust
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

Use `http::serve(...)` directly for the normal server case. Use `runtime::builder().run(...)` only when you need runtime configuration or want to scope other work around the server.

## Concept Mapping

| Go | Camber | Notes |
|---|---|---|
| `http.HandleFunc` | `router.get()` / `router.post()` | Explicit HTTP method |
| `http.HandleFunc("PUT", ...)` | `router.put()` | Also: `patch()`, `head()`, `options()`, `delete()` |
| `http.ListenAndServe` | `http::serve("addr", router)` | Blocks until shutdown. Add `runtime::builder().run(...)` only for runtime configuration |
| `r.URL.Path` | `req.path()` | URL path without query |
| `r.URL.Query().Get("k")` | `req.query("k")` | Returns `Option<&str>` |
| `r.PathValue("id")` | `req.param("id")` | Returns `Option<&str>` |
| `r.Header.Get("k")` | `req.header("k")` | Case-insensitive, O(1) lookup |
| `r.Cookie("name")` | `req.cookie("name")` | Returns `Option<&str>`, lazy-parsed |
| `r.FormFile("upload")` | `req.multipart()` | Returns `MultipartReader` with all parts |
| `r.FormValue("k")` | `req.form("k")` | URL-encoded body, lazy-parsed |
| `json.NewDecoder(r.Body)` | `req.json::<T>()` | Type-safe deserialization |
| `json.NewEncoder(w)` | `Response::json(200, &val)` | Status code explicit |
| `w.WriteHeader(code)` | `Response::text(code, msg)` | Status in constructor |
| `http.SetCookie(w, &cookie)` | `resp.set_cookie("n", "v")` | Also: `set_cookie_with()` for options |
| `go func() { ... }` | `spawn(\|\| { ... })` | Sync closure on blocking pool. Returns `JoinHandle` |
| `go func() { ... }` (async) | `spawn_async(async { ... })` | Async future on Tokio runtime. Returns `AsyncJoinHandle` |
| `make(chan T)` | `channel::new::<T>()` | Bounded (128 default) |
| `make(chan T, n)` | `channel::bounded::<T>(n)` | Explicit capacity |
| `ch <- value` | `tx.send(value)?` | Returns `Result`, no panic |
| `value := <-ch` | `rx.recv()?` | Returns `Result`, no panic |
| `select { case ... }` | `select! { val = rx => ... }` | Macro, same semantics |
| `context.Context` | `JoinHandle::cancel()` + channel cancellation | Cooperative, checked at IO |
| `interface{}` | Generics + `dyn Trait` | Compile-time or explicit dispatch |
| `defer` | RAII (Drop trait) | Automatic cleanup on scope exit |
| `error` | `Result<T, RuntimeError>` | Compiler enforces handling |
| `nil` | `Option<T>` | Compiler enforces checking |
| `database/sql.Open` | `sqlx::PgPool::connect(url)` | Use `sqlx` or your preferred ORM directly |
| `sql.DB` (pool) | `sqlx::PgPool` | Async pool, cloned into handlers as needed |
| `sql.DB.Query` | `sqlx::query(...).fetch_*(&pool).await` | Camber does not wrap query execution |

## Goroutines and Channels

### Go

```go
func fanOut(urls []string) []int {
    ch := make(chan int, len(urls))

    for i, url := range urls {
        go func(i int, url string) {
            resp, err := http.Get(url)
            if err != nil {
                ch <- 0
                return
            }
            ch <- resp.StatusCode
        }(i, url)
    }

    results := make([]int, len(urls))
    for i := range urls {
        results[i] = <-ch
    }
    return results
}
```

### Camber

```rust
async fn fan_out(urls: &[&str]) -> Vec<u16> {
    let (tx, rx) = channel::bounded::<u16>(urls.len());

    for url in urls {
        let tx = tx.clone();
        let url = url.to_string();
        spawn_async(async move {
            let status = match http::get(&url).await {
                Ok(r) => r.status(),
                Err(_) => 0,
            };
            let _ = tx.send(status);
        });
    }
    drop(tx);

    rx.iter().collect()
}
```

The pattern is identical: spawn work, send results through a channel, collect.

## HTTP Client

### Go

```go
resp, err := http.Get("https://api.example.com/items/1")
resp, err := http.Post("https://api.example.com/items", "application/json", body)

req, _ := http.NewRequest("PUT", "https://api.example.com/items/1", body)
resp, err := http.DefaultClient.Do(req)

req, _ := http.NewRequest("DELETE", "https://api.example.com/items/1", nil)
resp, err := http.DefaultClient.Do(req)
```

### Camber

```rust
let resp = http::get("https://api.example.com/items/1").await?;
let resp = http::post("https://api.example.com/items", &payload).await?;
let resp = http::put("https://api.example.com/items/1", &payload).await?;
let resp = http::delete("https://api.example.com/items/1").await?;
let resp = http::patch("https://api.example.com/items/1", &partial).await?;
```

All methods are first-class. No `NewRequest` / `Do` ceremony.

### HTTP Client with Retries

In Go, retry logic requires a custom `RoundTripper` or a third-party library like `hashicorp/go-retryablehttp`. Camber has it built in.

#### Go

```go
client := retryablehttp.NewClient()
client.RetryMax = 3
client.RetryWaitMin = 100 * time.Millisecond
resp, err := client.Get("https://api.example.com/items/1")
```

#### Camber

```rust
let c = http::client()
    .retries(3)
    .backoff(Duration::from_millis(100));
let resp = c.get("https://api.example.com/items/1").await?;
```

Retries transient errors (timeouts, connection failures, 429/502/503/504) with exponential backoff and jitter. The client is built once and reused across calls.

## Middleware

Go middleware is a chain of `func(http.Handler) http.Handler` wrappers. Camber uses `use_middleware()` with the same wrap-and-call pattern.

### Go

```go
func logging(next http.Handler) http.Handler {
    return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
        log.Println(r.Method, r.URL.Path)
        next.ServeHTTP(w, r)
    })
}

mux := http.NewServeMux()
handler := logging(cors(mux))
http.ListenAndServe(":8080", handler)
```

### Camber

```rust
let mut router = Router::new();

router.use_middleware(|req, next| {
    let method = req.method().to_owned();
    let path = req.path().to_owned();
    async move {
        tracing::info!("{} {}", method, path);
        next.call(req).await
    }
});

router.get("/hello", hello);
http::serve("0.0.0.0:8080", router)
```

Middleware registered first wraps all later middleware. Call `next.call(req).await` to continue the chain, or return early to short-circuit.
If you need request data after `.await`, copy owned values out of `req` before entering `async move`.

### CORS

Go CORS typically uses `rs/cors` or manual header injection. Camber provides `cors::allow_origins()`.

```rust
router.use_middleware(cors::allow_origins(&["https://app.example.com"]));
```

For fine-grained control, use the builder:

```rust
router.use_middleware(
    cors::builder()
        .origins(&["https://app.example.com"])
        .methods(&["GET", "POST", "PUT"])
        .headers(&["Content-Type", "Authorization"])
        .credentials()
        .build(),
);
```

Preflight OPTIONS requests are handled automatically.

### Compression

```rust
router.use_middleware(compression::auto());
```

Negotiates via `Accept-Encoding`. Gzips text responses over 1KB. Skips binary and already-compressed responses.

### Rate Limiting

```rust
router.use_middleware(rate_limit::per_second(100)?);
```

Token bucket algorithm. Returns 429 with `Retry-After` header when exhausted. Also available: `rate_limit::per_minute(n)` and `rate_limit::builder()` for burst configuration.

## Cookies

### Go

```go
// Read
cookie, err := r.Cookie("session")

// Write
http.SetCookie(w, &http.Cookie{
    Name:     "session",
    Value:    "abc123",
    Path:     "/",
    MaxAge:   3600,
    HttpOnly: true,
    Secure:   true,
})
```

### Camber

```rust
// Read
let session = req.cookie("session");

// Write (simple)
Response::text(200, "ok").set_cookie("session", "abc123")

// Write (with options)
let opts = CookieOptions::new()
    .path("/")
    .max_age(3600)
    .http_only()
    .secure();
Response::text(200, "ok").set_cookie_with("session", "abc123", &opts)
```

## File Uploads (Multipart)

### Go

```go
func upload(w http.ResponseWriter, r *http.Request) {
    r.ParseMultipartForm(32 << 20)
    file, header, err := r.FormFile("upload")
    if err != nil {
        http.Error(w, "bad upload", 400)
        return
    }
    defer file.Close()
    data, _ := io.ReadAll(file)
    fmt.Fprintf(w, "got %s (%d bytes)", header.Filename, len(data))
}
```

### Camber

```rust
router.post("/upload", |req| async move {
    let mp = req.multipart()?;
    for part in mp.parts() {
        let name = part.name();
        let filename = part.filename().unwrap_or("(none)");
        let size = part.data().len();
        tracing::info!("field={name} file={filename} size={size}");
    }
    Ok(Response::text(200, "uploaded"))
});
```

`req.multipart()` parses all parts at once. Each `Part` exposes `name()`, `filename()`, `content_type()`, and `data()`.

## WebSockets

### Go (gorilla/websocket)

```go
var upgrader = websocket.Upgrader{}

func echo(w http.ResponseWriter, r *http.Request) {
    conn, err := upgrader.Upgrade(w, r, nil)
    if err != nil { return }
    defer conn.Close()
    for {
        _, msg, err := conn.ReadMessage()
        if err != nil { break }
        conn.WriteMessage(websocket.TextMessage, msg)
    }
}
```

### Camber

```rust
router.ws("/echo", |_req, mut ws: WsConn| {
    while let Some(msg) = ws.recv() {
        ws.send(&msg)?;
    }
    Ok(())
});
```

No upgrade ceremony. The handler receives a `WsConn` with blocking `recv()` and `send()` methods. Also available: `recv_binary()`, `send_binary()`, and `recv_message()` for mixed text/binary.

Requires the `ws` feature flag.

## Database

### Go

```go
db, err := sql.Open("postgres", connStr)
db.SetMaxOpenConns(10)

rows, err := db.Query("SELECT id, name FROM users WHERE active = $1", true)
```

### Camber

```rust
let pool = sqlx::PgPool::connect(conn_str).await?;

let rows = sqlx::query("SELECT id, name FROM users WHERE active = $1")
    .bind(true)
    .fetch_all(&pool)
    .await?;
```

Camber does not ship a database wrapper. Use `sqlx` or your preferred ORM directly inside handlers and background tasks.

## Health Checks

Go health endpoints are hand-rolled. Camber generates `/health` automatically from registered resources.

### Go

```go
func health(w http.ResponseWriter, r *http.Request) {
    err := db.Ping()
    if err != nil {
        w.WriteHeader(503)
        json.NewEncoder(w).Encode(map[string]string{"db": "unhealthy"})
        return
    }
    w.WriteHeader(200)
    json.NewEncoder(w).Encode(map[string]string{"db": "healthy"})
}
```

### Camber

```rust
router.get("/health", |_req| async {
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => Response::json(200, &serde_json::json!({"db": "healthy"})),
        Err(_) => Response::json(503, &serde_json::json!({"db": "unhealthy"})),
    }
})
```

For async database clients like `sqlx`, keep the health check in normal application code unless you have a specific need for a custom runtime resource adapter.

## Error Handling

Go uses `if err != nil` at every call site. Camber uses `?` for propagation.

### Go

```go
func getUser(w http.ResponseWriter, r *http.Request) {
    body, err := io.ReadAll(r.Body)
    if err != nil {
        http.Error(w, "bad request", 400)
        return
    }

    var user User
    if err := json.Unmarshal(body, &user); err != nil {
        http.Error(w, "invalid json", 400)
        return
    }

    // ...
}
```

### Camber

```rust
router.post("/users", |req| async move {
    let user: User = req.json()?; // BadRequest on parse failure
    Ok(Response::json(200, &user))
});
```

`?` replaces every `if err != nil` block. Parse errors map to 400 automatically.

## What You Gain

- **No nil pointer panics.** `Option<T>` forces you to handle the absent case.
- **No data races.** The compiler prevents them at build time.
- **No garbage collector.** Predictable latency, lower memory.
- **Same deployment model.** Single static binary, same as Go.

## What Changes

- **Compile times are slower.** Incremental rebuilds are 2-3 seconds. Clean builds take longer. The tradeoff: once it compiles, it works.
- **Ownership is new.** You'll hit borrow checker errors. Camber minimizes this by keeping handlers simple (async closures that take a `Request` and return `Response`), but you'll learn ownership eventually. That's the point.
- **The ecosystem is different.** Go's `net/http` is standard library. Camber is a runtime built on Tokio. The Rust ecosystem is fragmented but powerful.
