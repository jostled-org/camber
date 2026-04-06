mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// ── Step 1.T1: middleware closure compiles and runs with standard boxing ──
#[camber::test]
async fn middleware_closure_runs_with_standard_boxing() {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let fut = next.call(req);
        Box::pin(async move { fut.await.with_header("X-Async-MW", "works") })
    });
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    let has_mw = resp
        .headers()
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-async-mw") && v.as_ref() == "works");
    assert!(
        has_mw,
        "missing X-Async-MW header, got: {:?}",
        resp.headers()
    );

    runtime::request_shutdown();
}

// ── Plan test 1.T3: async_middleware_wraps_handler ────────────────
#[camber::test]
async fn async_middleware_wraps_handler() {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let resp_fut = next.call(req);
        Box::pin(async move { resp_fut.await.with_header("X-MW", "present") })
            as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    let has_mw = resp
        .headers()
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-mw") && v.as_ref() == "present");
    assert!(has_mw, "missing X-MW header, got: {:?}", resp.headers());

    runtime::request_shutdown();
}

// ── Plan test 1.T4: async_middleware_short_circuits ───────────────
#[camber::test]
async fn async_middleware_short_circuits() {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let has_auth = req
            .headers()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        match has_auth {
            true => next.call(req),
            false => Box::pin(async { Response::text(401, "unauthorized").expect("valid status") }),
        }
    });
    router.get("/protected", |_req: &Request| async {
        Response::text(200, "secret")
    });

    let addr = common::spawn_server(router);

    // No Authorization header → 401
    let resp = http::get(&format!("http://{addr}/protected"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // With Authorization header → 200
    let raw = common::raw_request(addr, "GET", "/protected", &[("Authorization", "Bearer t")]);
    assert!(
        raw.starts_with("HTTP/1.1 200"),
        "expected 200 with auth, got: {raw}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn middleware_modifies_response() {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let resp_fut = next.call(req);
        Box::pin(async move { resp_fut.await.with_header("X-Request-Id", "abc-123") })
            as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");

    let has_request_id = resp
        .headers()
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-request-id") && v.as_ref() == "abc-123");
    assert!(
        has_request_id,
        "missing X-Request-Id header, got: {:?}",
        resp.headers()
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn middleware_ordering_preserved() {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let fut = next.call(req);
        Box::pin(async move { fut.await.with_header("X-Order", "A") })
            as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    router.use_middleware(|req: &Request, next| {
        let fut = next.call(req);
        Box::pin(async move { fut.await.with_header("X-Order", "B") })
            as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    router.get("/order", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/order")).await.unwrap();

    assert_eq!(resp.status(), 200);

    let order_headers: Vec<&str> = resp
        .headers()
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("x-order"))
        .map(|(_, v)| v.as_ref())
        .collect();

    assert!(
        order_headers.contains(&"A"),
        "missing X-Order: A, got: {order_headers:?}"
    );
    assert!(
        order_headers.contains(&"B"),
        "missing X-Order: B, got: {order_headers:?}"
    );

    // A is outermost, so its header is added last (after B's)
    assert_eq!(
        order_headers,
        vec!["B", "A"],
        "A (first registered, outermost) should execute after B"
    );

    runtime::request_shutdown();
}

#[test]
fn middleware_runs_for_sse_route() {
    common::test_runtime()
        .shutdown_timeout(std::time::Duration::from_secs(2))
        .run(|| {
            let invocations = Arc::new(AtomicUsize::new(0));
            let mw_counter = Arc::clone(&invocations);

            let mut router = Router::new();
            router.use_middleware(move |req: &Request, next| {
                mw_counter.fetch_add(1, Ordering::SeqCst);
                next.call(req)
            });
            router.get_sse(
                "/events",
                |_req: &Request, writer: &mut camber::http::SseWriter| {
                    writer.event("message", "hello")?;
                    Ok(())
                },
            );

            let addr = common::spawn_server(router);

            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();
            assert!(
                buf.starts_with("HTTP/1.1 200"),
                "expected 200 for SSE, got: {buf}"
            );

            assert!(
                invocations.load(Ordering::SeqCst) > 0,
                "middleware should have been invoked for SSE route"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn middleware_runs_for_stream_route() {
    common::test_runtime()
        .shutdown_timeout(std::time::Duration::from_secs(2))
        .run(|| {
            let invocations = Arc::new(AtomicUsize::new(0));
            let mw_counter = Arc::clone(&invocations);

            let mut router = Router::new();
            router.use_middleware(move |req: &Request, next| {
                mw_counter.fetch_add(1, Ordering::SeqCst);
                next.call(req)
            });
            router.get_stream("/download", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = camber::http::StreamResponse::new(200);
                    tokio::spawn(async move {
                        let _ = sender.send("data").await;
                    });
                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

            use std::io::{Read, Write};
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /download HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();
            assert!(
                buf.starts_with("HTTP/1.1 200"),
                "expected 200 for stream, got: {buf}"
            );

            assert!(
                invocations.load(Ordering::SeqCst) > 0,
                "middleware should have been invoked for stream route"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn middleware_with_handler_responds() {
    common::test_runtime()
        .run(|| {
            let mw_ran = Arc::new(AtomicBool::new(false));
            let mw_flag = Arc::clone(&mw_ran);

            let mut router = Router::new();
            router.use_middleware(move |req: &Request, next| {
                mw_flag.store(true, Ordering::Release);
                let fut = next.call(req);
                Box::pin(async move { fut.await.with_header("X-Mw-Check", "ran") })
                    as Pin<Box<dyn Future<Output = Response> + Send>>
            });
            router.get("/check", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let raw = common::raw_request(addr, "GET", "/check", &[]);

            assert!(raw.starts_with("HTTP/1.1 200"), "expected 200, got: {raw}");
            let raw_lower = raw.to_lowercase();
            assert!(
                raw_lower.contains("x-mw-check: ran"),
                "middleware header missing, got: {raw}"
            );
            assert!(
                mw_ran.load(Ordering::Acquire),
                "middleware should have executed"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
