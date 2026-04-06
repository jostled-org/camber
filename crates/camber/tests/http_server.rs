mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn request_from_net_preserves_data() {
    runtime::run(|| {
        let method_ok = Arc::new(AtomicBool::new(false));
        let path_ok = Arc::new(AtomicBool::new(false));
        let header_ok = Arc::new(AtomicBool::new(false));

        let m = Arc::clone(&method_ok);
        let p = Arc::clone(&path_ok);
        let h = Arc::clone(&header_ok);

        let mut router = Router::new();
        router.get("/test-path", move |req: &Request| {
            let m = Arc::clone(&m);
            let p = Arc::clone(&p);
            let h = Arc::clone(&h);
            let is_get = req.method() == "GET";
            let is_path = req.path() == "/test-path";
            let found = req
                .headers()
                .any(|(k, v)| k.eq_ignore_ascii_case("x-foo") && v == "bar");
            async move {
                if is_get {
                    m.store(true, Ordering::Release);
                }
                if is_path {
                    p.store(true, Ordering::Release);
                }
                if found {
                    h.store(true, Ordering::Release);
                }
                Response::text(200, "ok")
            }
        });

        let listener = camber::net::listen("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr").tcp().unwrap();
        spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });

        // Send raw HTTP request with custom header
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        let raw =
            "GET /test-path HTTP/1.1\r\nHost: localhost\r\nX-Foo: bar\r\nConnection: close\r\n\r\n";
        stream.write_all(raw.as_bytes()).expect("write");

        // Read response
        let mut buf = [0u8; 4096];
        let mut response = Vec::new();
        loop {
            match std::io::Read::read(&mut stream, &mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("read error: {e}"),
            }
        }

        let resp = String::from_utf8_lossy(&response);
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");

        assert!(method_ok.load(Ordering::Acquire), "method mismatch");
        assert!(path_ok.load(Ordering::Acquire), "path mismatch");
        assert!(header_ok.load(Ordering::Acquire), "header not found");

        runtime::request_shutdown();
    })
    .unwrap();
}

#[test]
fn request_method_returns_correct_str_for_all_methods() {
    let methods = [
        ("GET", "GET"),
        ("POST", "POST"),
        ("PUT", "PUT"),
        ("DELETE", "DELETE"),
        ("PATCH", "PATCH"),
        ("HEAD", "HEAD"),
        ("OPTIONS", "OPTIONS"),
    ];
    for (input, expected) in methods {
        let req = Request::builder()
            .method(input)
            .expect("valid method")
            .finish()
            .expect("valid request");
        assert_eq!(req.method(), expected, "method mismatch for {input}");
    }
}

#[test]
fn request_headers_preserve_valid_utf8() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.get("/headers", |req: &Request| {
            let val = req.header("x-test").unwrap_or("missing").to_owned();
            async move { Response::text(200, &val) }
        });

        let addr = common::spawn_server(router);

        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set timeout");
        let raw = b"GET /headers HTTP/1.1\r\nHost: localhost\r\nX-Test: caf\xC3\xA9\r\nConnection: close\r\n\r\n";
        stream.write_all(raw).expect("write");

        let mut buf = [0u8; 4096];
        let mut response = Vec::new();
        loop {
            match std::io::Read::read(&mut stream, &mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    break;
                }
                Err(e) => panic!("read error: {e}"),
            }
        }

        let resp = String::from_utf8_lossy(&response);
        assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
        assert!(
            resp.contains("café"),
            "expected valid UTF-8 header value preserved"
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn http_server_accepts_connections_after_refactor() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/refactor-check", |_req: &Request| async {
                Response::text(200, "works")
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set timeout");
            let raw =
                "GET /refactor-check HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            stream.write_all(raw.as_bytes()).expect("write");

            let mut buf = [0u8; 4096];
            let mut response = Vec::new();
            loop {
                match std::io::Read::read(&mut stream, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => response.extend_from_slice(&buf[..n]),
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        break;
                    }
                    Err(e) => panic!("read error: {e}"),
                }
            }

            let resp = String::from_utf8_lossy(&response);
            assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
            assert!(resp.contains("works"), "expected body 'works'");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn request_empty_query_string_is_empty() {
    let req = Request::builder()
        .path("/hello")
        .finish()
        .expect("valid request");
    assert_eq!(req.query("anything"), None);
}

#[test]
fn response_text_body_accessible_as_str_and_bytes() {
    let resp = Response::text(200, "hello").expect("valid status");
    assert_eq!(resp.body(), "hello");
    assert_eq!(resp.body_bytes(), b"hello");
    assert_eq!(resp.status(), 200);

    // Verify body is consistent across multiple calls
    assert_eq!(resp.body(), "hello");
    assert_eq!(resp.body_bytes(), b"hello");
}

#[test]
fn response_text_body_round_trips_through_server() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/text-body", |_req: &Request| async {
                Response::text(200, "hello from text")
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set timeout");
            let raw = "GET /text-body HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            stream.write_all(raw.as_bytes()).expect("write");

            let mut buf = [0u8; 4096];
            let mut response = Vec::new();
            loop {
                match std::io::Read::read(&mut stream, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => response.extend_from_slice(&buf[..n]),
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        break;
                    }
                    Err(e) => panic!("read error: {e}"),
                }
            }

            let resp = String::from_utf8_lossy(&response);
            assert!(resp.contains("200 OK"), "expected 200, got: {resp}");
            assert!(
                resp.contains("hello from text"),
                "expected body in response"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn handler_arc_clone_produces_same_response() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "Hello, world!");

    runtime::request_shutdown();
}

#[camber::test]
async fn async_handler_runs_on_worker_thread() {
    let mut router = Router::new();
    router.get("/thread-check", |_req: &Request| async {
        let name = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        Response::text(200, &name)
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/thread-check"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let thread_name = resp.body();
    assert!(
        thread_name.contains("tokio-runtime-worker"),
        "handler should run on worker thread; got: {thread_name}"
    );

    runtime::request_shutdown();
}

// ── Plan test 1.T1: async_handler_hello_text ─────────────────────
#[camber::test]
async fn async_handler_hello_text() {
    let mut router = Router::new();
    router.get("/", |_req: &Request| async { Response::text(200, "hello") });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    runtime::request_shutdown();
}

// ── Plan test 1.T2: async_handler_returns_result ─────────────────
#[camber::test]
async fn async_handler_returns_result() {
    let mut router = Router::new();
    router.get("/fail", |_req: &Request| async {
        Err(RuntimeError::Http("fail".into())) as Result<Response, RuntimeError>
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/fail")).await.unwrap();

    assert_eq!(resp.status(), 500);

    runtime::request_shutdown();
}

// ── Plan test 1.T5: all_http_methods_register_async ──────────────
#[camber::test]
async fn all_http_methods_register_async() {
    let mut router = Router::new();
    router.get("/get", |_req: &Request| async {
        Response::text(200, "get")
    });
    router.post("/post", |_req: &Request| async {
        Response::text(200, "post")
    });
    router.put("/put", |_req: &Request| async {
        Response::text(200, "put")
    });
    router.delete("/delete", |_req: &Request| async {
        Response::text(200, "delete")
    });
    router.patch("/patch", |_req: &Request| async {
        Response::text(200, "patch")
    });
    router.head("/head", |_req: &Request| async { Response::empty(200) });
    router.options("/options", |_req: &Request| async {
        Response::text(200, "options")
    });

    let addr = common::spawn_server(router);

    let get = http::get(&format!("http://{addr}/get")).await.unwrap();
    assert_eq!(get.status(), 200);
    assert_eq!(get.body(), "get");

    let post = http::post(&format!("http://{addr}/post"), "")
        .await
        .unwrap();
    assert_eq!(post.status(), 200);

    let put = http::put(&format!("http://{addr}/put"), "").await.unwrap();
    assert_eq!(put.status(), 200);

    let delete = http::delete(&format!("http://{addr}/delete"))
        .await
        .unwrap();
    assert_eq!(delete.status(), 200);

    let patch = http::patch(&format!("http://{addr}/patch"), "")
        .await
        .unwrap();
    assert_eq!(patch.status(), 200);

    let head = http::head(&format!("http://{addr}/head")).await.unwrap();
    assert_eq!(head.status(), 200);

    let options = http::options(&format!("http://{addr}/options"))
        .await
        .unwrap();
    assert_eq!(options.status(), 200);

    runtime::request_shutdown();
}

// ── Plan test 1.T6: handler_with_path_params ─────────────────────
#[camber::test]
async fn handler_with_path_params() {
    let mut router = Router::new();
    router.get("/users/:id", |req: &Request| {
        let id = req.param("id").unwrap_or("missing").to_owned();
        async move { Response::text(200, &id) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/users/42")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "42");

    runtime::request_shutdown();
}

// ── Migrated from async_handlers.rs ──────────────────────────────

#[test]
fn async_handler_concurrent_requests() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/slow", |_req: &Request| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Response::text(200, "done")
            });

            let addr = common::spawn_server(router);

            let start = std::time::Instant::now();
            let handles: Vec<_> = (0..20)
                .map(|_| {
                    std::thread::spawn(move || {
                        use std::io::{Read, Write};
                        let mut stream = std::net::TcpStream::connect(addr).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        stream.write_all(
                        b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    ).unwrap();
                        let mut buf = String::new();
                        stream.read_to_string(&mut buf).unwrap();
                        buf
                    })
                })
                .collect();

            let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            let elapsed = start.elapsed();

            for resp in &results {
                assert!(
                    resp.starts_with("HTTP/1.1 200"),
                    "expected 200, got: {resp}"
                );
            }

            assert!(
                elapsed < Duration::from_millis(500),
                "concurrent requests took {elapsed:?}, expected < 500ms (proves no thread pinning)"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn async_handler_result_ok_responds() {
    let mut router = Router::new();
    router.get("/result", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/result")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");

    runtime::request_shutdown();
}

#[camber::test]
async fn handler_panic_kills_connection() {
    let mut router = Router::new();
    router.get("/panic", |_req: &Request| async {
        #[allow(unreachable_code)]
        {
            panic!("intentional test panic");
            Response::empty(500)
        }
    });

    let addr = common::spawn_server(router);

    let result = http::get(&format!("http://{addr}/panic")).await;
    assert!(
        result.is_err(),
        "panicking handler should kill the connection, got status: {}",
        result.map(|r| r.status()).unwrap_or(0),
    );

    runtime::request_shutdown();
}

#[test]
fn request_oncelock_lazy_fields_work() {
    let req = Request::builder()
        .path("/test?foo=bar&baz=qux")
        .body("{\"key\": \"value\"}")
        .finish()
        .expect("valid request");

    // Query params lazily parsed
    assert_eq!(req.query("foo"), Some("bar"));
    assert_eq!(req.query("baz"), Some("qux"));
    assert_eq!(req.query("missing"), None);

    // Body lazily decoded
    assert_eq!(req.body(), "{\"key\": \"value\"}");

    // JSON deserialization works
    let parsed: std::collections::HashMap<String, String> = req.json().unwrap();
    assert_eq!(parsed.get("key").map(String::as_str), Some("value"));
}
