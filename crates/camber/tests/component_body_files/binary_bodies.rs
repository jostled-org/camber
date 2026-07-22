use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::time::Duration;

#[test]
fn handler_receives_binary_body_bytes() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/binary", |req: &Request| {
                let body = req.body_bytes().to_vec();
                async move { Response::bytes(200, body) }
            });
            let addr = common::spawn_server(router);

            let body: Vec<u8> = (0u8..=255).collect();
            let response = crate::http::request(
                addr,
                "POST",
                "/binary",
                &[("Content-Type", "application/octet-stream")],
                &body,
                Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), body.as_slice());

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn handler_body_text_is_backward_compatible() {
    let mut router = Router::new();
    router.post("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let addr = common::spawn_server(router);

    let resp = http::post(&format!("http://{addr}/echo"), "hello world")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello world");

    runtime::request_shutdown();
}

#[test]
fn response_bytes_sends_binary_content() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/png-header", |_req: &Request| async {
                Response::bytes(200, vec![0x89, 0x50, 0x4E, 0x47])
            });
            let addr = common::spawn_server(router);

            let response =
                crate::http::request(addr, "GET", "/png-header", &[], &[], Duration::from_secs(5))
                    .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), &[0x89, 0x50, 0x4E, 0x47]);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn json_parsing_works_with_bytes_model() {
    let mut router = Router::new();
    router.post("/parse", |req: &Request| {
        let result: Result<serde_json::Value, RuntimeError> = req.json();
        async move {
            result
                .and_then(|value| Response::text(200, value["name"].as_str().unwrap_or("missing")))
        }
    });
    let addr = common::spawn_server(router);

    let resp = http::post_json(&format!("http://{addr}/parse"), r#"{"name":"camber"}"#)
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "camber");

    runtime::request_shutdown();
}
