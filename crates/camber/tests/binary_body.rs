mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::io::{Read, Write};

#[test]
fn handler_receives_binary_body_bytes() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.post("/binary", |req: &Request| {
            let len = req.body_bytes().len().to_string();
            async move { Response::text(200, &len) }
        });
        let addr = common::spawn_server(router);

        // Build a 256-byte body with every byte value 0x00..0xFF
        let body: Vec<u8> = (0u8..=255).collect();
        let request = format!(
            "POST /binary HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );

        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(&body).unwrap();

        let mut buf = String::new();
        stream.read_to_string(&mut buf).unwrap();
        assert!(buf.contains("256"), "response was: {buf}");

        runtime::request_shutdown();
    }).unwrap();
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

            // Use raw TCP to read the binary response
            let request =
                "GET /png-header HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream.write_all(request.as_bytes()).unwrap();

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).unwrap();

            // Find the body after the HTTP header delimiter
            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("no header delimiter found");
            let body = &buf[header_end + 4..];
            assert_eq!(body, &[0x89, 0x50, 0x4E, 0x47]);

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
            result.and_then(|value| {
                Response::text(
                    200,
                    &value["name"].as_str().unwrap_or("missing").to_string(),
                )
            })
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
