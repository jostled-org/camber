use crate::error::BenchError;
use crate::servers::ServerHandle;
use axum::extract::Path;
use axum::response::IntoResponse;
use axum::routing::get;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

fn bind_and_spawn(app: axum::Router) -> Result<(SocketAddr, ServerHandle), BenchError> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    listener.set_nonblocking(true)?;

    let thread = spawn_axum_runtime(listener, app);

    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok((addr, ServerHandle::new(thread)))
}

/// Spawn a tokio runtime matching camber's parallelism and serve the axum app.
/// Used by both in-process tests (via `bind_and_spawn`) and standalone binary.
pub fn spawn_axum_runtime(
    listener: std::net::TcpListener,
    app: axum::Router,
) -> std::thread::JoinHandle<()> {
    let worker_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async {
            let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                return;
            };
            let _ = axum::serve(listener, app).await;
        });
    })
}

// --- Router builders (shared by in-process tests and standalone binaries) ---

fn hello_text_app() -> axum::Router {
    axum::Router::new().route("/", get(|| async { "Hello, world!" }))
}

fn hello_json_app() -> axum::Router {
    axum::Router::new().route(
        "/",
        get(|| async { axum::Json(serde_json::json!({"message": "Hello, world!"})) }),
    )
}

fn path_param_app() -> axum::Router {
    axum::Router::new().route(
        "/users/{id}",
        get(|Path(id): Path<String>| async move { format!("User {id}") }),
    )
}

fn static_file_app() -> axum::Router {
    axum::Router::new().route(
        "/",
        get(|| async {
            (
                [(axum::http::header::CONTENT_TYPE, "text/html")],
                super::STATIC_HTML,
            )
                .into_response()
        }),
    )
}

fn db_query_app(upstream: SocketAddr) -> axum::Router {
    let upstream_url: Arc<str> = format!("http://{upstream}/").into();
    let client = reqwest::Client::new();
    axum::Router::new().route(
        "/query",
        get(move || {
            let client = client.clone();
            let url = Arc::clone(&upstream_url);
            async move {
                let _ = client.get(&*url).send().await;
                axum::Json(
                    serde_json::json!({"id": 1, "name": "Alice", "email": "alice@example.com"}),
                )
            }
        }),
    )
}

fn middleware_stack_app(upstream: SocketAddr) -> Result<axum::Router, BenchError> {
    use tower_http::compression::CompressionLayer;
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let origin =
        "http://example.com"
            .parse()
            .map_err(|e: axum::http::header::InvalidHeaderValue| {
                BenchError::ServerStart(e.to_string().into_boxed_str())
            })?;
    let cors = CorsLayer::new().allow_origin(AllowOrigin::exact(origin));
    let rate_limiter = TokenBucket::new(1_000_000);

    let upstream_url: Arc<str> = format!("http://{upstream}/").into();
    let client = reqwest::Client::new();
    Ok(axum::Router::new()
        .route(
            "/",
            get(move || {
                let client = client.clone();
                let url = Arc::clone(&upstream_url);
                async move {
                    let _ = client.get(&*url).send().await;
                    axum::Json(serde_json::json!({"status": "ok"}))
                }
            }),
        )
        .layer(cors)
        .layer(CompressionLayer::new())
        .layer(axum::middleware::from_fn(
            move |req, next: axum::middleware::Next| {
                let limiter = rate_limiter.clone();
                async move {
                    match limiter.try_acquire() {
                        true => next.run(req).await,
                        false => axum::http::StatusCode::TOO_MANY_REQUESTS.into_response(),
                    }
                }
            },
        )))
}

fn proxy_forward_app(upstream: SocketAddr) -> axum::Router {
    let backend: Arc<str> = format!("http://{upstream}").into();
    let client = reqwest::Client::new();
    axum::Router::new().route(
        "/",
        get(move || {
            let backend = Arc::clone(&backend);
            let client = client.clone();
            async move {
                match client.get(&*backend).send().await {
                    Ok(resp) => {
                        let body = resp.text().await.unwrap_or_default();
                        (
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            body,
                        )
                            .into_response()
                    }
                    Err(_) => axum::http::StatusCode::BAD_GATEWAY.into_response(),
                }
            }
        }),
    )
}

fn fan_out_app(upstream: SocketAddr) -> axum::Router {
    let url: Arc<str> = format!("http://{upstream}/").into();
    let urls: Arc<[Arc<str>; 3]> = Arc::new([Arc::clone(&url), Arc::clone(&url), Arc::clone(&url)]);
    fan_out_app_from_urls(urls)
}

fn fan_out_app_from_urls(urls: Arc<[Arc<str>; 3]>) -> axum::Router {
    let client = reqwest::Client::new();
    axum::Router::new().route(
        "/fan-out",
        get(move || {
            let urls = Arc::clone(&urls);
            let client = client.clone();
            async move { fan_out_handler_axum(&client, &urls).await }
        }),
    )
}

async fn fan_out_handler_axum(
    client: &reqwest::Client,
    urls: &[Arc<str>; 3],
) -> axum::response::Response {
    let futures: Box<[_]> = urls
        .iter()
        .map(|url| {
            let client = client.clone();
            let url = Arc::clone(url);
            tokio::spawn(async move {
                client
                    .get(&*url)
                    .send()
                    .await
                    .ok()
                    .map(|r| async { r.text().await.unwrap_or_default() })
            })
        })
        .collect();

    let mut results = Vec::with_capacity(3);
    for fut in futures {
        match fut.await {
            Ok(Some(text_fut)) => results.push(text_fut.await),
            _ => results.push(String::from(r#"{"error":"failed"}"#)),
        }
    }
    let merged = format!("[{}]", results.join(","));
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        merged,
    )
        .into_response()
}

/// Build an axum app for the given benchmark name. Used by standalone binaries.
pub fn build_app(bench: &str, upstream: Option<SocketAddr>) -> Result<axum::Router, BenchError> {
    match bench {
        "hello_text" => Ok(hello_text_app()),
        "hello_json" => Ok(hello_json_app()),
        "path_param" => Ok(path_param_app()),
        "static_file" => Ok(static_file_app()),
        "db_query" => Ok(db_query_app(super::require_upstream(bench, upstream)?)),
        "middleware_stack" => middleware_stack_app(super::require_upstream(bench, upstream)?),
        "proxy_forward" => Ok(proxy_forward_app(super::require_upstream(bench, upstream)?)),
        "fan_out" => Ok(fan_out_app(super::require_upstream(bench, upstream)?)),
        _ => Err(BenchError::ServerStart(
            format!("unknown axum benchmark: {bench}").into_boxed_str(),
        )),
    }
}

// --- In-process starters (used by smoke tests and tier runners) ---

pub fn start_hello_text() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(hello_text_app())
}

pub fn start_hello_json() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(hello_json_app())
}

pub fn start_path_param() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(path_param_app())
}

pub fn start_static_file() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(static_file_app())
}

pub fn start_db_query(upstream_addr: SocketAddr) -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(db_query_app(upstream_addr))
}

pub fn start_middleware_stack(
    upstream_addr: SocketAddr,
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(middleware_stack_app(upstream_addr)?)
}

/// Atomic token bucket rate limiter — matches camber's per_second(100_000).
/// Refills tokens based on elapsed time since last refill.
#[derive(Clone)]
struct TokenBucket {
    state: Arc<TokenBucketState>,
}

struct TokenBucketState {
    tokens: AtomicU64,
    last_refill: std::sync::Mutex<Instant>,
    rate: u64,
}

impl TokenBucket {
    fn new(per_second: u64) -> Self {
        Self {
            state: Arc::new(TokenBucketState {
                tokens: AtomicU64::new(per_second),
                last_refill: std::sync::Mutex::new(Instant::now()),
                rate: per_second,
            }),
        }
    }

    fn try_acquire(&self) -> bool {
        self.refill();
        loop {
            let current = self.state.tokens.load(Ordering::Relaxed);
            let exchanged = self.state.tokens.compare_exchange_weak(
                current,
                current.saturating_sub(1),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            match (current, exchanged) {
                (0, _) => return false,
                (_, Ok(_)) => return true,
                (_, Err(_)) => continue,
            }
        }
    }

    fn refill(&self) {
        let Ok(mut last) = self.state.last_refill.lock() else {
            return;
        };
        let now = Instant::now();
        let elapsed = now.duration_since(*last).as_secs_f64();
        let new_tokens = (elapsed * self.state.rate as f64) as u64;
        if new_tokens > 0 {
            *last = now;
            let current = self.state.tokens.load(Ordering::Relaxed);
            let capped = (current + new_tokens).min(self.state.rate);
            self.state.tokens.store(capped, Ordering::Relaxed);
        }
    }
}

pub fn start_proxy_forward(
    upstream_addr: SocketAddr,
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(proxy_forward_app(upstream_addr))
}

pub fn start_fan_out(
    upstream_addrs: &[SocketAddr; 3],
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    let urls: Arc<[Arc<str>; 3]> = Arc::new(std::array::from_fn(|i| {
        format!("http://{}/", upstream_addrs[i]).into()
    }));
    bind_and_spawn(fan_out_app_from_urls(urls))
}
