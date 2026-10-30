use crate::runtime_support as common;

use camber::http::ClientResponse;
use camber::http::{self, Bytes, Request, Response, Router};
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

    let resp: ClientResponse = http::post(&format!("http://{addr}/echo"), "hello world")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "hello world");

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

    let resp: ClientResponse =
        http::post_json(&format!("http://{addr}/parse"), r#"{"name":"camber"}"#)
            .await
            .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "camber");

    runtime::request_shutdown();
}

#[test]
fn shared_body_survives_request_and_shares_storage() {
    let payload = Bytes::from(vec![0, 255, 128, 13, 10]);
    let request = Request::builder()
        .body_raw(payload.clone())
        .finish()
        .unwrap();
    let shared = request.body_shared();
    assert_eq!(shared.as_ptr(), request.body_bytes().as_ptr());
    assert_eq!(shared.as_ptr(), payload.as_ptr());
    assert_eq!(shared.clone().as_ptr(), shared.as_ptr());
    drop(request);
    assert_eq!(shared.as_ref(), &[0, 255, 128, 13, 10]);
    assert_eq!(shared.as_ptr(), payload.as_ptr());
}

#[test]
fn shared_body_moves_into_awaited_blocking_work_without_copying() {
    runtime::run(|| {
        runtime::block_on(async {
            let request = Request::builder()
                .body_raw(vec![0, 255, 128, 13, 10])
                .finish()
                .unwrap();
            let shared = request.body_shared();
            let original = shared.clone();
            drop(request);
            let processed = camber::spawn(move || {
                let checksum = shared.iter().map(|byte| u64::from(*byte)).sum::<u64>();
                (shared, checksum)
            })
            .await
            .unwrap();
            assert_eq!(processed.0.as_ptr(), original.as_ptr());
            assert_eq!(processed.1, 406);
        })
    })
    .unwrap();
}

#[test]
fn routed_handler_moves_shared_body_into_awaited_blocking_work() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/checksum", |req: &Request| {
                let body = req.body_shared();
                let shares_request_storage = body.as_ptr() == req.body_bytes().as_ptr();
                async move {
                    let original = body.clone();
                    let (processed, checksum) = camber::spawn(move || {
                        let checksum = body.iter().map(|byte| u64::from(*byte)).sum::<u64>();
                        (body, checksum)
                    })
                    .await?;
                    let moved = processed.as_ptr() == original.as_ptr();
                    Response::text(200, &format!("{shares_request_storage} {moved} {checksum}"))
                }
            });
            let addr = common::spawn_server(router);

            let body: Vec<u8> = (0u8..=255).collect();
            let response = crate::http::request(
                addr,
                "POST",
                "/checksum",
                &[("Content-Type", "application/octet-stream")],
                &body,
                Duration::from_secs(5),
            )
            .unwrap();
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"true true 32640");

            runtime::request_shutdown();
        })
        .unwrap();
}
