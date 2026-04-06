#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Minimal isolation test: camber async handler vs bare hyper async handler vs axum.
//! Proves framework dispatch overhead by hitting a shared upstream mock.
//! This is a diagnostic tool, not a benchmark — use camber-bench for real numbers.

use std::io::Write;
use std::time::{Duration, Instant};

type HyperResponse = hyper::Response<http_body_util::Full<bytes::Bytes>>;

fn main() {
    // Start upstream mock
    let upstream = camber_bench::servers::upstream::start().unwrap();
    println!("Upstream at {upstream}");

    // Run camber server hitting upstream
    let camber_addr = start_camber(upstream);
    println!("Camber at {camber_addr}");

    // Run bare hyper server hitting same upstream
    let hyper_addr = start_bare_hyper(upstream);
    println!("Bare hyper at {hyper_addr}");

    // Run axum server hitting same upstream
    let axum_addr = start_axum(upstream);
    println!("Axum at {axum_addr}");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    println!("\nWarmup...");
    rt.block_on(load("camber", camber_addr, 50, Duration::from_secs(2)));
    rt.block_on(load("bare_hyper", hyper_addr, 50, Duration::from_secs(2)));
    rt.block_on(load("axum", axum_addr, 50, Duration::from_secs(2)));

    println!("\nBenchmark (10s, 100 connections):");
    rt.block_on(load("camber", camber_addr, 100, Duration::from_secs(10)));
    rt.block_on(load("bare_hyper", hyper_addr, 100, Duration::from_secs(10)));
    rt.block_on(load("axum", axum_addr, 100, Duration::from_secs(10)));
}

async fn load(name: &str, addr: std::net::SocketAddr, connections: u32, duration: Duration) {
    let client = reqwest::Client::new();
    let url = format!("http://{addr}/query");
    let deadline = Instant::now() + duration;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Duration>();

    let mut handles = Vec::with_capacity(connections as usize);
    for _ in 0..connections {
        let client = client.clone();
        let url = url.clone();
        let tx = tx.clone();
        handles.push(tokio::spawn(async move {
            while Instant::now() < deadline {
                let start = Instant::now();
                let _ = client.get(&url).send().await;
                let _ = tx.send(start.elapsed());
            }
        }));
    }
    drop(tx);

    let mut count = 0u64;
    while rx.recv().await.is_some() {
        count += 1;
    }
    for h in handles {
        let _ = h.await;
    }

    let elapsed = duration.as_secs_f64();
    let rps = count as f64 / elapsed;
    println!("  {name}: {rps:.0} req/s ({count} total)");
}

fn start_camber(upstream: std::net::SocketAddr) -> std::net::SocketAddr {
    let upstream_url: std::sync::Arc<str> = format!("http://{upstream}/").into();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Err(e) = run_camber_runtime(tx, upstream_url) {
            let _ = writeln!(std::io::stderr(), "runtime error: {e}");
        }
    });
    rx.recv().unwrap()
}

fn run_camber_runtime(
    tx: std::sync::mpsc::Sender<std::net::SocketAddr>,
    upstream_url: std::sync::Arc<str>,
) -> Result<(), camber::RuntimeError> {
    camber::runtime::builder()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| run_camber_server(tx, upstream_url))
}

fn run_camber_server(
    tx: std::sync::mpsc::Sender<std::net::SocketAddr>,
    upstream_url: std::sync::Arc<str>,
) {
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(200)
        .build()
        .unwrap();
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    tx.send(addr).unwrap();
    let mut router = camber::http::Router::new();
    register_camber_query_route(&mut router, client, upstream_url);
    let _ = camber::http::serve_listener(listener, router);
}

fn start_bare_hyper(upstream: std::net::SocketAddr) -> std::net::SocketAddr {
    let upstream_url: std::sync::Arc<str> = format!("http://{upstream}/").into();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .pool_max_idle_per_host(200)
                .build()
                .unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let client = client.clone();
                let url = std::sync::Arc::clone(&upstream_url);
                tokio::spawn(async move {
                    serve_bare_hyper_connection(stream, client, url).await;
                });
            }
        });
    });
    std::thread::sleep(Duration::from_millis(50));
    addr
}

fn register_camber_query_route(
    router: &mut camber::http::Router,
    client: reqwest::Client,
    upstream_url: std::sync::Arc<str>,
) {
    router.get("/query", move |_: &camber::http::Request| {
        camber_query_response(client.clone(), std::sync::Arc::clone(&upstream_url))
    });
}

async fn camber_query_response(
    client: reqwest::Client,
    url: std::sync::Arc<str>,
) -> Result<camber::http::Response, camber::RuntimeError> {
    let _ = client.get(url.as_ref()).send().await;
    camber::http::Response::json(200, &serde_json::json!({"id": 1, "name": "Alice"}))
}

async fn serve_bare_hyper_query(
    client: reqwest::Client,
    url: std::sync::Arc<str>,
) -> Result<HyperResponse, std::convert::Infallible> {
    let _ = client.get(&*url).send().await;
    Ok(make_hyper_json_response())
}

async fn serve_bare_hyper_connection(
    stream: tokio::net::TcpStream,
    client: reqwest::Client,
    url: std::sync::Arc<str>,
) {
    let service = hyper::service::service_fn(move |_: hyper::Request<hyper::body::Incoming>| {
        serve_bare_hyper_query(client.clone(), std::sync::Arc::clone(&url))
    });
    let io = hyper_util::rt::TokioIo::new(stream);
    let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
        .serve_connection(io, service)
        .await;
}

fn make_hyper_json_response() -> HyperResponse {
    let body = serde_json::to_vec(&serde_json::json!({"id": 1, "name": "Alice"})).unwrap();
    hyper::Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap()
}

fn start_axum(upstream: std::net::SocketAddr) -> std::net::SocketAddr {
    use axum::routing::get;
    let upstream_url: std::sync::Arc<str> = format!("http://{upstream}/").into();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let client = reqwest::Client::builder()
                .pool_max_idle_per_host(200)
                .build()
                .unwrap();
            let app = axum::Router::new().route(
                "/query",
                get(move || {
                    let client = client.clone();
                    let url = std::sync::Arc::clone(&upstream_url);
                    async move {
                        let _ = client.get(&*url).send().await;
                        axum::Json(serde_json::json!({"id": 1, "name": "Alice"}))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    std::thread::sleep(Duration::from_millis(50));
    addr
}
