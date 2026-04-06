mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Payload {
    name: Box<str>,
    age: u32,
}

#[test]
fn json_request_and_response_round_trip() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/echo", |req: &Request| {
                let result: Result<Payload, RuntimeError> = req.json();
                async move { result.and_then(|payload| Response::json(200, &payload)) }
            });

            let addr = common::spawn_server(router);

            let body = r#"{"name":"alice","age":30}"#;
            let resp =
                common::block_on(http::post_json(&format!("http://{addr}/echo"), body)).unwrap();

            assert_eq!(resp.status(), 200);

            let ct = resp
                .headers()
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"));
            assert_eq!(
                ct.map(|(_, v)| v.as_ref()),
                Some("application/json"),
                "Content-Type should be application/json"
            );

            let parsed: Payload = serde_json::from_str(resp.body()).unwrap();
            assert_eq!(
                parsed,
                Payload {
                    name: "alice".into(),
                    age: 30
                }
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn response_json_body_accessible() {
    let data = Payload {
        name: "bob".into(),
        age: 25,
    };
    let resp = Response::json(200, &data).expect("valid status");
    assert_eq!(resp.status(), 200);

    let body = resp.body();
    let parsed: Payload = serde_json::from_str(body).unwrap();
    assert_eq!(parsed.name.as_ref(), "bob");
    assert_eq!(parsed.age, 25);

    // body_bytes matches body as UTF-8
    assert_eq!(resp.body_bytes(), body.as_bytes());
}

#[test]
fn json_deserialization_error_returns_400() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/typed", |req: &Request| {
                let result: Result<Payload, RuntimeError> = req.json();
                async move { result.and_then(|_payload| Response::text(200, "ok")) }
            });

            let addr = common::spawn_server(router);

            // Missing required "age" field
            let body = r#"{"name":"alice"}"#;
            let resp =
                common::block_on(http::post_json(&format!("http://{addr}/typed"), body)).unwrap();

            assert_eq!(resp.status(), 400);

            runtime::request_shutdown();
        })
        .unwrap();
}
