#![allow(clippy::unwrap_used)]

mod common;

use camber::http::{self, Request, Response, Router, otel};
use camber::runtime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Send a GET request with a custom header using reqwest async client.
fn get_with_header(
    addr: std::net::SocketAddr,
    path: &str,
    header_name: &str,
    header_value: &str,
) -> (u16, String) {
    common::block_on(async {
        let resp = reqwest::Client::new()
            .get(format!("http://{addr}{path}"))
            .header(header_name, header_value)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap();
        (status, body)
    })
}

#[camber::test]
async fn otel_middleware_creates_span_for_request() {
    let mut router = Router::new();
    router.use_middleware(otel::tracing());
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");

    runtime::request_shutdown();
}

#[test]
fn otel_extracts_incoming_traceparent() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(otel::tracing());
            router.get("/trace", |_req: &Request| async {
                match otel::current_traceparent() {
                    Some(tp) => Response::text(200, &tp),
                    None => Response::text(500, "no trace context"),
                }
            });

            let addr = common::spawn_server(router);
            let trace_id = "0af7651916cd43dd8448eb211c80319c";
            let parent_id = "b7ad6b7169203331";
            let traceparent = format!("00-{trace_id}-{parent_id}-01");

            let (status, body) = get_with_header(addr, "/trace", "traceparent", &traceparent);

            assert_eq!(status, 200);
            // The middleware generates a new span_id but preserves the trace_id
            assert!(
                body.contains(trace_id),
                "response should contain original trace_id, got: {body}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn otel_injects_traceparent_on_outbound_calls() {
    common::test_runtime()
        .run(|| {
            // Upstream server records whether it received a traceparent header
            let received_traceparent: Arc<Mutex<Option<Box<str>>>> = Arc::new(Mutex::new(None));
            let recorder = Arc::clone(&received_traceparent);

            let mut upstream = Router::new();
            upstream.get("/upstream", move |req: &Request| {
                let tp = req.header("traceparent").map(Box::from);
                *recorder.lock().unwrap_or_else(|e| e.into_inner()) = tp;
                async { Response::text(200, "upstream-ok") }
            });
            let upstream_addr = common::spawn_server(upstream);

            // Main server with otel middleware — handler calls upstream
            let mut router = Router::new();
            router.use_middleware(otel::tracing());
            let upstream_url: Box<str> = format!("http://{upstream_addr}/upstream").into();
            router.get("/proxy", move |_req: &Request| {
                let upstream_url = upstream_url.clone();
                async move {
                    let _ = http::get(&upstream_url).await;
                    Response::text(200, "ok")
                }
            });
            let addr = common::spawn_server(router);

            let trace_id = "0af7651916cd43dd8448eb211c80319c";
            let traceparent = format!("00-{trace_id}-b7ad6b7169203331-01");
            let (status, _) = get_with_header(addr, "/proxy", "traceparent", &traceparent);
            assert_eq!(status, 200);

            // Verify upstream received a traceparent with the same trace_id
            let tp = received_traceparent
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            assert!(tp.is_some(), "upstream should receive traceparent header");
            let tp_val = tp.unwrap();
            assert!(
                tp_val.contains(trace_id),
                "outbound traceparent should contain original trace_id, got: {tp_val}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

/// 3.T2: Handler without incoming traceparent gets a generated trace context.
#[test]
fn otel_generates_span_id_for_async_handler() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(otel::tracing());
            router.get("/trace", |_req: &Request| async {
                match otel::current_traceparent() {
                    Some(tp) => Response::text(200, &tp),
                    None => Response::text(500, "no trace context"),
                }
            });

            let addr = common::spawn_server(router);
            // Send without traceparent — middleware should generate one
            let resp = common::block_on(http::get(&format!("http://{addr}/trace"))).unwrap();

            assert_eq!(resp.status(), 200);
            let body = resp.body();
            // Generated traceparent: 00-{32hex}-{16hex}-{2hex} = 55 chars
            assert_eq!(body.len(), 55, "expected 55-char traceparent, got: {body}");
            assert!(
                body.starts_with("00-"),
                "traceparent should start with version 00, got: {body}"
            );
            // Verify trace_id is not all zeros (generated)
            let trace_id = &body[3..35];
            assert_ne!(
                trace_id, "00000000000000000000000000000000",
                "generated trace_id should not be all zeros"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn otel_disabled_has_zero_overhead() {
    common::test_runtime()
        .run(|| {
            // Upstream server records whether it received a traceparent header
            let received_traceparent = Arc::new(AtomicBool::new(false));
            let recorder = Arc::clone(&received_traceparent);

            let mut upstream = Router::new();
            upstream.get("/upstream", move |req: &Request| {
                let has_tp = req.header("traceparent").is_some();
                let recorder = Arc::clone(&recorder);
                async move {
                    match has_tp {
                        true => recorder.store(true, Ordering::Release),
                        false => {}
                    }
                    Response::text(200, "ok")
                }
            });
            let upstream_addr = common::spawn_server(upstream);

            // NO otel middleware — handler calls upstream
            let mut router = Router::new();
            let upstream_url: Box<str> = format!("http://{upstream_addr}/upstream").into();
            router.get("/proxy", move |_req: &Request| {
                let upstream_url = upstream_url.clone();
                async move {
                    let _ = http::get(&upstream_url).await;
                    Response::text(200, "ok")
                }
            });
            let addr = common::spawn_server(router);

            let resp = common::block_on(http::get(&format!("http://{addr}/proxy"))).unwrap();
            assert_eq!(resp.status(), 200);

            // Upstream should NOT have received a traceparent header
            assert!(
                !received_traceparent.load(Ordering::Acquire),
                "outbound call should not have traceparent without otel middleware"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
