mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn e2e_full_v02_runtime() {
    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend server returning "backend-data"
            let mut backend_router = Router::new();
            backend_router.get("/data", |_req: &Request| async {
                Response::text(200, "backend-data")
            });
            let backend_listener = camber::net::listen("127.0.0.1:0").unwrap();
            let backend_addr = backend_listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                http::serve_listener(backend_listener, backend_router)
            });

            // Main server with three routes
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
            let main_listener = camber::net::listen("127.0.0.1:0").unwrap();
            let main_addr = main_listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                http::serve_listener(main_listener, router)
            });

            // Warm up
            let resp = common::block_on(http::get(&format!("http://{main_addr}/hello"))).unwrap();
            assert_eq!(resp.status(), 200);

            // Send 50 concurrent requests across all 3 routes
            let hello_count = Arc::new(AtomicUsize::new(0));
            let proxy_count = Arc::new(AtomicUsize::new(0));
            let slow_count = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();

            for i in 0..50 {
                let route = match i % 3 {
                    0 => "/hello",
                    1 => "/proxy",
                    _ => "/slow",
                };
                let url = format!("http://{main_addr}{route}");
                let hello_c = Arc::clone(&hello_count);
                let proxy_c = Arc::clone(&proxy_count);
                let slow_c = Arc::clone(&slow_count);

                let h = spawn(move || {
                    let resp = common::block_on(http::get(&url)).unwrap();
                    assert_eq!(resp.status(), 200);
                    match route {
                        "/hello" => {
                            assert_eq!(resp.body(), "Hello");
                            hello_c.fetch_add(1, Ordering::SeqCst);
                        }
                        "/proxy" => {
                            assert!(
                                resp.body().contains("backend-data"),
                                "proxy response: {}",
                                resp.body()
                            );
                            proxy_c.fetch_add(1, Ordering::SeqCst);
                        }
                        "/slow" => {
                            assert_eq!(resp.body(), "slow-done");
                            slow_c.fetch_add(1, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                });
                handles.push(h);
            }

            for h in handles {
                h.join().unwrap();
            }

            assert_eq!(hello_count.load(Ordering::SeqCst), 17);
            assert_eq!(proxy_count.load(Ordering::SeqCst), 17);
            assert_eq!(slow_count.load(Ordering::SeqCst), 16);

            // Verify keep-alive
            verify_keepalive(main_addr);

            runtime::request_shutdown();
        })
        .unwrap();
}

fn verify_keepalive(addr: std::net::SocketAddr) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let req1 = "GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
    stream.write_all(req1.as_bytes()).unwrap();
    let resp1 = read_http_response(&mut stream);
    assert!(resp1.contains("200 OK"), "first: {resp1}");
    assert!(resp1.contains("Hello"), "first body: {resp1}");

    let req2 = "GET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(req2.as_bytes()).unwrap();
    let resp2 = read_http_response(&mut stream);
    assert!(resp2.contains("200 OK"), "second: {resp2}");
    assert!(resp2.contains("Hello"), "second body: {resp2}");
}

fn read_http_response(stream: &mut TcpStream) -> String {
    let mut buf = [0u8; 4096];
    let mut response = Vec::new();

    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                response.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&response);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let headers = &text[..header_end];
                    let body_start = header_end + 4;
                    if let Some(cl) = extract_content_length(headers) {
                        let body_len = response.len() - body_start;
                        if body_len >= cl {
                            break;
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => panic!("read error: {e}"),
        }
    }

    String::from_utf8_lossy(&response).into_owned()
}

fn extract_content_length(headers: &str) -> Option<usize> {
    for line in headers.lines() {
        let lower = line.to_lowercase();
        if let Some(val) = lower.strip_prefix("content-length:") {
            return val.trim().parse().ok();
        }
    }
    None
}
