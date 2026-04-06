# Middleware Reference

Camber middleware wraps route handlers in registration order.

## Model

- The first middleware registered is the outermost wrapper.
- Middleware receives `(&Request, Next)`.
- Continue the chain with `next.call(req).await`.
- Short-circuit by returning a response directly.

For normal HTTP handlers, middleware wraps the full owned request/response path.
For gRPC, `proxy_stream(...)`, WebSocket upgrades, SSE, and internal routes, middleware may run as
a request gate before the streaming or head-only transport path begins.

```rust
use camber::http::{Request, Router};

let mut router = Router::new();
router.use_middleware(|req, next| {
    let method = req.method().to_owned();
    let path = req.path().to_owned();
    async move {
        let resp = next.call(req).await;
        tracing::info!("{} {} -> {}", method, path, resp.status());
        resp
    }
});
```

## Ownership Rule

The same ownership rule as handlers applies here: if you need request data after `.await`, move owned values into the async block first.

```rust
use camber::http::{IntoResponse, Response};

router.use_middleware(|req, next| {
    let authorized = req.header("authorization").is_some_and(valid);
    async move {
        match authorized {
            true => next.call(req).await,
            false => Response::text(401, "unauthorized")?.into_response(),
        }
    }
});
```

## Built-In Middleware

### CORS

Quick form:

```rust
use camber::http::cors;

router.use_middleware(cors::allow_origins(&["https://app.example.com"]));
```

Builder form:

```rust
use camber::http::cors;

router.use_middleware(
    cors::builder()
        .origins(&["https://app.example.com"])
        .methods(&["GET", "POST"])
        .headers(&["Content-Type", "Authorization"])
        .credentials()
        .build(),
);
```

### Compression

```rust
use camber::http::compression;

router.use_middleware(compression::auto());
```

Gzips eligible text responses over 1 KB.

### Rate Limiting

```rust
use camber::http::rate_limit;

router.use_middleware(rate_limit::per_second(100)?);
router.use_middleware(rate_limit::per_minute(1_000)?);
```

Builder form is available for token/burst control.

### Validation

```rust
use camber::http::validate;

router.use_middleware(validate::json::<CreateUser>());
```

Validates JSON before the handler runs.

## Streaming And Gate-Only Paths

Camber uses one user-facing middleware API, but not every transport shape is wrapped the same way.

- Buffered HTTP handlers: full middleware chain around the owned `Request`
- `proxy_stream(...)`: middleware can inspect params, headers, and connection metadata, then allow or reject before the upstream stream starts
- gRPC: middleware gates on request metadata before tonic receives the streaming body
- WebSocket, SSE, and internal routes: middleware can still short-circuit, but the framework does not buffer request bodies these routes do not use
