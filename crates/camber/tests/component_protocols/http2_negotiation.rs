use std::future::{Future, IntoFuture};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;

use crate::runtime_support;

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(5);

async fn bounded<T>(future: impl Future<Output = T>, operation: &str) -> T {
    tokio::time::timeout(PROTOCOL_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("HTTP/2 {operation} timed out"))
}

async fn h2c_get(addr: std::net::SocketAddr, path: &str) -> (u16, Box<[u8]>) {
    bounded(
        async {
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let (mut client, connection) = h2::client::handshake(tcp).await.unwrap();
            let connection = tokio::spawn(connection);
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
            drop(client);
            // The response is complete. Abort the client driver rather than waiting for
            // the server's independent keepalive policy to close the connection.
            connection.abort();
            match connection.await {
                Ok(Ok(())) => {}
                Err(error) if error.is_cancelled() => {}
                Ok(Err(error)) => panic!("HTTP/2 client driver failed: {error}"),
                Err(error) => panic!("HTTP/2 client driver join failed: {error}"),
            }
            (status, body_bytes.into_boxed_slice())
        },
        "request",
    )
    .await
}

#[test]
fn http2_cleartext_request() {
    runtime_support::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_: &Request| async { Response::text(200, "hi") });
            let addr = runtime_support::spawn_server(router);
            let (status, body) = runtime_support::block_on(h2c_get(addr, "/hello"));
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"hi");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn http1_and_http2_same_port() {
    let mut router = Router::new();
    router.get("/hello", |_: &Request| async { Response::text(200, "hi") });
    let addr = runtime_support::spawn_server(router);

    let response = bounded(
        camber::http::get(&format!("http://{addr}/hello")),
        "HTTP/1.1 request",
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), "hi");

    let (status, body) = h2c_get(addr, "/hello").await;
    assert_eq!(status, 200);
    assert_eq!(body.as_ref(), b"hi");
    runtime::request_shutdown();
}

#[camber::test]
async fn graceful_http2_sends_goaway_drains_stream_and_then_joins() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_release = Arc::clone(&release);

    let mut router = Router::new();
    router.get("/retained", move |_: &Request| {
        let entered_tx = Arc::clone(&entered_tx);
        let release = Arc::clone(&handler_release);
        async move {
            let sender = entered_tx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            if let Some(sender) = sender {
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
    let tcp = bounded(tokio::net::TcpStream::connect(addr), "connect")
        .await
        .unwrap();
    let (mut client, connection) = bounded(h2::client::handshake(tcp), "handshake")
        .await
        .unwrap();
    let connection = tokio::spawn(connection);

    let request = ::http::Request::get(format!("http://{addr}/retained"))
        .version(::http::Version::HTTP_2)
        .body(())
        .unwrap();
    client = bounded(client.ready(), "client readiness").await.unwrap();
    let (retained_response, _) = client.send_request(request, true).unwrap();
    bounded(entered_rx, "handler entry").await.unwrap();

    runtime::request_shutdown();
    let new_stream_rejection = bounded(
        std::future::poll_fn(|cx| match client.poll_ready(cx) {
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(error),
            std::task::Poll::Ready(Ok(())) | std::task::Poll::Pending => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }),
        "GOAWAY",
    )
    .await;
    assert!(new_stream_rejection.is_go_away());
    assert!(new_stream_rejection.is_remote());
    assert_eq!(new_stream_rejection.reason(), Some(h2::Reason::NO_ERROR));

    let mut completion = Box::pin(handle.into_future());
    if let Some(result) = completion.as_mut().now_or_never() {
        release.add_permits(1);
        panic!("ServerHandle completed while the accepted stream was retained: {result:?}");
    }

    release.add_permits(1);
    let response = bounded(retained_response, "retained response")
        .await
        .expect("retained HTTP/2 stream was not drained");
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let mut body_bytes = Vec::new();
    while let Some(chunk) = bounded(body.data(), "retained body frame").await {
        let chunk = chunk.unwrap();
        body_bytes.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len()).unwrap();
    }
    assert_eq!(body_bytes, b"drained");

    let result: Result<(), RuntimeError> = bounded(completion, "server join").await;
    assert!(result.is_ok(), "graceful ServerHandle result: {result:?}");
    bounded(connection, "connection join")
        .await
        .unwrap()
        .unwrap();
}
