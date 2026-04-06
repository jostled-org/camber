mod common;

use camber::http::{Request, Response, Router};
use camber::runtime;
use std::time::Duration;

/// Send an HTTP/2 cleartext (h2c) GET request using the h2 crate (prior knowledge).
/// Returns (status_code, body_string).
async fn h2c_get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut client, conn) = h2::client::handshake(tcp).await.unwrap();

    tokio::spawn(async move {
        conn.await.unwrap();
    });

    let request = ::http::Request::get(format!("http://{addr}{path}"))
        .body(())
        .unwrap();

    let (response, _) = client.send_request(request, true).unwrap();
    let response = response.await.unwrap();
    let status = response.status().as_u16();

    let mut body = response.into_body();
    let mut body_bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        body_bytes.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }

    (status, String::from_utf8(body_bytes).unwrap())
}

#[test]
fn http2_cleartext_request() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hi")
            });

            let addr = common::spawn_server(router);

            let (status, body) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(h2c_get(addr, "/hello"))
            });

            assert_eq!(status, 200);
            assert_eq!(body, "hi");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn http1_and_http2_same_port() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hi")
    });

    let addr = common::spawn_server(router);

    // HTTP/1.1
    let resp = camber::http::get(&format!("http://{addr}/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hi");

    // HTTP/2 cleartext on the same port
    let (status, body) = h2c_get(addr, "/hello").await;

    assert_eq!(status, 200);
    assert_eq!(body, "hi");

    runtime::request_shutdown();
}
