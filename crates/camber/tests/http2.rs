mod common;

use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;
use std::future::IntoFuture;
use std::sync::{Arc, Mutex};
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

// 1.T4
#[camber::test]
async fn graceful_http2_sends_goaway_drains_stream_and_then_joins() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_release = Arc::clone(&release);

    let mut router = Router::new();
    router.get("/retained", move |_req: &Request| {
        let entered_tx = Arc::clone(&entered_tx);
        let release = Arc::clone(&handler_release);
        async move {
            if let Some(sender) = entered_tx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
            let permit = release.acquire().await.unwrap();
            drop(permit);
            Response::text(200, "drained")
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, router);
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut client, connection) = h2::client::handshake(tcp).await.unwrap();
    let connection = tokio::spawn(connection);

    let request = ::http::Request::get(format!("http://{addr}/retained"))
        .version(::http::Version::HTTP_2)
        .body(())
        .unwrap();
    client = client.ready().await.unwrap();
    let (retained_response, _) = client.send_request(request, true).unwrap();
    entered_rx.await.unwrap();

    runtime::request_shutdown();
    let new_stream_rejection = tokio::time::timeout(
        Duration::from_secs(5),
        std::future::poll_fn(|cx| match client.poll_ready(cx) {
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(error),
            std::task::Poll::Ready(Ok(())) | std::task::Poll::Pending => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }),
    )
    .await
    .expect("timed out waiting for HTTP/2 GOAWAY");
    assert!(new_stream_rejection.is_go_away());
    assert!(new_stream_rejection.is_remote());
    assert_eq!(new_stream_rejection.reason(), Some(h2::Reason::NO_ERROR));

    let mut completion = Box::pin(handle.into_future());
    if let Some(result) = completion.as_mut().now_or_never() {
        release.add_permits(1);
        panic!("ServerHandle completed while the accepted stream was retained: {result:?}");
    }

    release.add_permits(1);
    let response = tokio::time::timeout(Duration::from_secs(5), retained_response)
        .await
        .expect("timed out waiting for retained HTTP/2 response")
        .expect("retained HTTP/2 stream was not drained");
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut body_bytes = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.unwrap();
        body_bytes.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }
    assert_eq!(body_bytes, b"drained");

    let result: Result<(), RuntimeError> = tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .expect("ServerHandle did not join after the retained stream completed");
    assert!(result.is_ok(), "graceful ServerHandle result: {result:?}");
    connection.await.unwrap().unwrap();
}
