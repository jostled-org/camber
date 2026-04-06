use crate::error::BenchError;
use crate::servers::{ServerHandle, bind_and_spawn, bind_listener_and_send_addr};
use std::net::SocketAddr;
use std::sync::Arc;

// --- Router builders (shared by in-process tests and standalone binaries) ---

fn hello_text_router() -> camber::http::Router {
    let mut router = camber::http::Router::new();
    router.get("/", |_: &camber::http::Request| async {
        camber::http::Response::text(200, "Hello, world!")
    });
    router
}

fn hello_json_router() -> camber::http::Router {
    let mut router = camber::http::Router::new();
    router.get("/", |_: &camber::http::Request| async {
        camber::http::Response::json(200, &serde_json::json!({"message": "Hello, world!"}))
    });
    router
}

fn path_param_router() -> camber::http::Router {
    let mut router = camber::http::Router::new();
    router.get("/users/:id", |req: &camber::http::Request| {
        let id = req.param("id").unwrap_or("unknown").to_owned();
        async move { camber::http::Response::text(200, &format!("User {id}")) }
    });
    router
}

fn static_file_router() -> camber::http::Router {
    let mut router = camber::http::Router::new();
    router.get("/", |_: &camber::http::Request| async {
        camber::http::Response::text(200, super::STATIC_HTML)
            .map(|r| r.with_content_type("text/html"))
    });
    router
}

fn db_query_router(upstream: SocketAddr) -> camber::http::Router {
    let upstream_url: Arc<str> = format!("http://{upstream}/").into();
    let mut router = camber::http::Router::new();
    router.get("/query", move |_: &camber::http::Request| {
        let url = Arc::clone(&upstream_url);
        async move {
            match camber::http::get(&url).await {
                Ok(_) => camber::http::Response::json(
                    200,
                    &serde_json::json!({"id": 1, "name": "Alice", "email": "alice@example.com"}),
                ),
                Err(_) => camber::http::Response::text(502, "upstream failed"),
            }
        }
    });
    router
}

fn middleware_stack_router(upstream: SocketAddr) -> Result<camber::http::Router, BenchError> {
    let upstream_url: Arc<str> = format!("http://{upstream}/").into();
    let rate_limiter = camber::http::rate_limit::per_second(1_000_000)
        .map_err(|e| BenchError::ServerStart(e.to_string().into_boxed_str()))?;
    let mut router = camber::http::Router::new();
    let cors = camber::http::cors::builder()
        .origins(&["http://example.com"])
        .build();
    router.use_middleware(cors);
    router.use_middleware(camber::http::compression::auto());
    router.use_middleware(rate_limiter);
    router.get("/", move |_: &camber::http::Request| {
        let url = Arc::clone(&upstream_url);
        async move {
            let _ = camber::http::get(&url).await;
            camber::http::Response::json(200, &serde_json::json!({"status": "ok"}))
        }
    });
    Ok(router)
}

fn proxy_forward_router(upstream: SocketAddr) -> camber::http::Router {
    let backend: Arc<str> = format!("http://{upstream}").into();
    let mut router = camber::http::Router::new();
    router.proxy("/", &backend);
    router
}

fn fan_out_router(upstream: SocketAddr) -> camber::http::Router {
    let url: Arc<str> = format!("http://{upstream}/").into();
    let urls: Arc<[Arc<str>; 3]> = Arc::new([Arc::clone(&url), Arc::clone(&url), Arc::clone(&url)]);
    let mut router = camber::http::Router::new();
    router.get("/fan-out", move |_: &camber::http::Request| {
        let urls = Arc::clone(&urls);
        async move { fan_out_handler(&urls).await }
    });
    router
}

/// Build a router for the given benchmark name. Used by standalone binaries.
pub fn build_router(
    bench: &str,
    upstream: Option<SocketAddr>,
) -> Result<camber::http::Router, BenchError> {
    match bench {
        "hello_text" => Ok(hello_text_router()),
        "hello_json" => Ok(hello_json_router()),
        "path_param" => Ok(path_param_router()),
        "static_file" => Ok(static_file_router()),
        "db_query" => Ok(db_query_router(super::require_upstream(bench, upstream)?)),
        "middleware_stack" => middleware_stack_router(super::require_upstream(bench, upstream)?),
        "proxy_forward" => Ok(proxy_forward_router(super::require_upstream(
            bench, upstream,
        )?)),
        "fan_out" => Ok(fan_out_router(super::require_upstream(bench, upstream)?)),
        _ => Err(BenchError::ServerStart(
            format!("unknown benchmark: {bench}").into_boxed_str(),
        )),
    }
}

// --- In-process starters (used by smoke tests and tier runners) ---

pub fn start_hello_text() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(|tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let _ = camber::http::serve_listener(listener, hello_text_router());
    })
}

pub fn start_hello_json() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(|tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let _ = camber::http::serve_listener(listener, hello_json_router());
    })
}

pub fn start_path_param() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(|tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let _ = camber::http::serve_listener(listener, path_param_router());
    })
}

pub fn start_static_file() -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(|tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let _ = camber::http::serve_listener(listener, static_file_router());
    })
}

pub fn start_db_query(upstream_addr: SocketAddr) -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(move |tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let _ = camber::http::serve_listener(listener, db_query_router(upstream_addr));
    })
}

pub fn start_middleware_stack(
    upstream_addr: SocketAddr,
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(move |tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let router = match middleware_stack_router(upstream_addr) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let _ = camber::http::serve_listener(listener, router);
    })
}

pub fn start_proxy_forward(
    upstream_addr: SocketAddr,
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    bind_and_spawn(move |tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let _ = camber::http::serve_listener(listener, proxy_forward_router(upstream_addr));
    })
}

pub fn start_fan_out(
    upstream_addrs: &[SocketAddr; 3],
) -> Result<(SocketAddr, ServerHandle), BenchError> {
    let urls: Arc<[Arc<str>; 3]> = Arc::new(std::array::from_fn(|i| {
        format!("http://{}/", upstream_addrs[i]).into()
    }));
    bind_and_spawn(move |tx| {
        let Some(listener) = bind_listener_and_send_addr(&tx) else {
            return;
        };
        let mut router = camber::http::Router::new();
        let urls = Arc::clone(&urls);
        router.get("/fan-out", move |_: &camber::http::Request| {
            let urls = Arc::clone(&urls);
            async move { fan_out_handler(&urls).await }
        });
        let _ = camber::http::serve_listener(listener, router);
    })
}

async fn fan_out_handler(
    urls: &[Arc<str>; 3],
) -> Result<camber::http::Response, camber::RuntimeError> {
    let mut handles = Vec::with_capacity(3);
    for url in urls {
        let url = Arc::clone(url);
        handles.push(camber::spawn_async(async move {
            let resp = camber::http::get(&url).await?;
            Ok(resp.body().to_owned())
        }));
    }

    let mut results = Vec::with_capacity(3);
    for handle in handles {
        let body: Result<String, camber::RuntimeError> = handle.await?;
        results.push(body?);
    }

    let merged = format!("[{}]", results.join(","));
    camber::http::Response::text(200, &merged).map(|r| r.with_content_type("application/json"))
}
