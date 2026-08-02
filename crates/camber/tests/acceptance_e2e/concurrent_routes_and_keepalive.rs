use crate::common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// The bound on one whole response frame, and on the socket reads it is made of.
const RESPONSE_BOUND: Duration = Duration::from_secs(5);

#[derive(Clone)]
struct RouteCounts {
    hello: Arc<AtomicUsize>,
    proxy: Arc<AtomicUsize>,
    slow: Arc<AtomicUsize>,
}

impl RouteCounts {
    fn new() -> Self {
        Self {
            hello: Arc::new(AtomicUsize::new(0)),
            proxy: Arc::new(AtomicUsize::new(0)),
            slow: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn record(&self, route: &str, response: &Response) {
        match route {
            "/hello" => {
                assert_eq!(response.body(), "Hello");
                self.hello.fetch_add(1, Ordering::SeqCst);
            }
            "/proxy" => {
                assert!(
                    response.body().contains("backend-data"),
                    "proxy response: {}",
                    response.body()
                );
                self.proxy.fetch_add(1, Ordering::SeqCst);
            }
            "/slow" => {
                assert_eq!(response.body(), "slow-done");
                self.slow.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }

    fn assert_expected(&self) {
        assert_eq!(self.hello.load(Ordering::SeqCst), 17);
        assert_eq!(self.proxy.load(Ordering::SeqCst), 17);
        assert_eq!(self.slow.load(Ordering::SeqCst), 16);
    }
}

fn spawn_backend_server() -> SocketAddr {
    let mut router = Router::new();
    router.get("/data", |_req: &Request| async {
        Response::text(200, "backend-data")
    });
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });
    addr
}

fn spawn_main_server(backend_addr: SocketAddr) -> SocketAddr {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello")
    });
    let backend_url = format!("http://{backend_addr}/data");
    router.get("/proxy", move |_req: &Request| {
        let backend_url = backend_url.clone();
        async move {
            match http::get(&backend_url).await {
                Ok(resp) => Response::text(200, resp.body()),
                Err(_) => Response::text(502, "upstream error"),
            }
        }
    });
    router.get("/slow", |_req: &Request| async {
        thread::sleep(Duration::from_millis(100));
        Response::text(200, "slow-done")
    });
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });
    addr
}

fn route_for_request(index: usize) -> &'static str {
    match index % 3 {
        0 => "/hello",
        1 => "/proxy",
        _ => "/slow",
    }
}

fn run_concurrent_routes(main_addr: SocketAddr) {
    let counts = RouteCounts::new();
    let handles: Vec<_> = (0..50)
        .map(|index| {
            let route = route_for_request(index);
            let url = format!("http://{main_addr}{route}");
            let request_counts = counts.clone();
            spawn(move || {
                let response = common::block_on(http::get(&url)).unwrap();
                assert_eq!(response.status(), 200);
                request_counts.record(route, &response);
            })
        })
        .collect();
    handles
        .into_iter()
        .for_each(|handle| handle.join().unwrap());
    counts.assert_expected();
}

fn run_full_runtime_journey() {
    let backend_addr = spawn_backend_server();
    let main_addr = spawn_main_server(backend_addr);
    // Warm up
    let response = common::block_on(http::get(&format!("http://{main_addr}/hello"))).unwrap();
    assert_eq!(response.status(), 200);
    run_concurrent_routes(main_addr);
    verify_keepalive(main_addr);
    runtime::request_shutdown();
}

#[test]
fn e2e_full_v02_runtime() {
    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(run_full_runtime_journey)
        .unwrap();
}

fn verify_keepalive(addr: std::net::SocketAddr) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(RESPONSE_BOUND)).unwrap();

    let req1 = "GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(req1.as_bytes()).unwrap();
    let resp1 = crate::http::read_http_response_bounded(&mut stream).unwrap();
    assert_eq!(resp1.status, 200);
    assert_eq!(resp1.body.as_ref(), b"Hello");

    let req2 = "GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req2.as_bytes()).unwrap();
    let resp2 = crate::http::read_http_response_bounded(&mut stream).unwrap();
    assert_eq!(resp2.status, 200);
    assert_eq!(resp2.body.as_ref(), b"Hello");

    let mut trailing = [0_u8; 1];
    assert_eq!(stream.read(&mut trailing).unwrap(), 0);
}
