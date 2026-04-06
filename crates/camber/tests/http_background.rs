use std::time::Duration;

use camber::http::{Request, Response, Router};

#[camber::test]
async fn serve_background_handles_request() {
    let mut router = Router::new();
    router.get("/ping", |_req: &Request| async {
        Response::text(200, "pong")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = camber::http::serve_background(listener, router);

    // Give the server a moment to start accepting
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("http://{addr}/ping")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "pong");
}

#[camber::test]
async fn serve_background_stops_on_cancel() {
    let mut router = Router::new();
    router.get("/ping", |_req: &Request| async {
        Response::text(200, "pong")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, router);

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify server is alive
    let resp = reqwest::get(format!("http://{addr}/ping")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Cancel the server
    handle.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Connection should fail now
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let result = client.get(format!("http://{addr}/ping")).send().await;
    assert!(result.is_err());
}
