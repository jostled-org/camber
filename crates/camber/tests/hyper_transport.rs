mod common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[test]
fn tokio_runtime_runs_existing_closure() {
    let result = runtime::run(|| 42).unwrap();
    assert_eq!(result, 42);

    let result = runtime::run(|| spawn(|| 1).join().unwrap()).unwrap();
    assert_eq!(result, 1);
}

#[test]
fn spawn_runs_on_tokio_blocking_pool() {
    runtime::run(|| {
        let mut handles = Vec::new();
        for _ in 0..10 {
            handles.push(spawn(|| {
                // Sleep briefly so the blocking pool allocates multiple threads
                // instead of reusing one for all near-instant tasks.
                std::thread::sleep(Duration::from_millis(10));
                std::thread::current().id()
            }));
        }

        let thread_ids: HashSet<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert!(
            thread_ids.len() >= 2,
            "expected at least 2 distinct thread IDs, got {}",
            thread_ids.len()
        );
    })
    .unwrap();
}

#[camber::test]
async fn hyper_serves_get_request() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hi")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hi");

    runtime::request_shutdown();
}

#[camber::test]
async fn hyper_serves_post_with_body() {
    let mut router = Router::new();
    router.post("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });

    let addr = common::spawn_server(router);

    let resp = http::post(&format!("http://{addr}/echo"), "payload")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "payload");

    runtime::request_shutdown();
}

#[test]
fn hyper_keepalive_reuses_connection() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = Router::new();
            router.get("/ping", |_req: &Request| async {
                Response::text(200, "pong")
            });

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

            // First request
            let req1 = "GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n";
            stream.write_all(req1.as_bytes()).unwrap();
            let resp1 = read_http_response(&mut stream);
            assert!(resp1.contains("200"), "first request failed: {resp1}");
            assert!(resp1.contains("pong"), "first body missing: {resp1}");

            // Second request on same connection
            let req2 = "GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            stream.write_all(req2.as_bytes()).unwrap();
            let resp2 = read_http_response(&mut stream);
            assert!(resp2.contains("200"), "second request failed: {resp2}");
            assert!(resp2.contains("pong"), "second body missing: {resp2}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn hyper_graceful_shutdown() {
    let mut router = Router::new();
    router.get("/alive", |_req: &Request| async {
        Response::text(200, "yes")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/alive")).await.unwrap();
    assert_eq!(resp.status(), 200);

    runtime::request_shutdown();
    // If we reach here, runtime exited cleanly
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
                        if response.len() - body_start >= cl {
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
    headers.lines().find_map(|line| {
        line.to_lowercase()
            .strip_prefix("content-length:")
            .and_then(|v| v.trim().parse().ok())
    })
}
