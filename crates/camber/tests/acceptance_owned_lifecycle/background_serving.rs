use std::time::Duration;

use camber::http::{Request, Response, Router};

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

#[camber::test]
async fn serve_background_handles_request() {
    let mut router = Router::new();
    router.get("/ping", |_req: &Request| async {
        Response::text(200, "pong")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    let response =
        tokio::time::timeout(EVENT_TIMEOUT, reqwest::get(format!("http://{addr}/ping"))).await;
    handle.shutdown();
    let server_result = tokio::time::timeout(EVENT_TIMEOUT, handle).await;
    assert!(server_result.is_ok(), "background server did not join");
    assert!(server_result.unwrap().is_ok(), "background server failed");
    let resp = response.unwrap().unwrap();
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
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");

    let response =
        tokio::time::timeout(EVENT_TIMEOUT, reqwest::get(format!("http://{addr}/ping"))).await;

    handle.cancel();
    let server_result = tokio::time::timeout(EVENT_TIMEOUT, handle).await;
    assert!(server_result.is_ok(), "cancelled server did not join");
    assert!(matches!(
        server_result.unwrap(),
        Err(camber::RuntimeError::Cancelled)
    ));

    let resp = response.unwrap().unwrap();
    assert_eq!(resp.status(), 200);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let result = client.get(format!("http://{addr}/ping")).send().await;
    assert!(result.is_err());
}
