use std::future::IntoFuture;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;

use crate::h2_client::{drain_h2_body, h2_request};
use crate::http::{HttpResponse, bounded};
use crate::runtime_support;

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Send one cleartext HTTP/2 `GET` and read the whole answer.
///
/// The authority is the address itself, which is what an `h2c` peer talking to a
/// bare socket has to say for itself.
async fn h2c_get(addr: std::net::SocketAddr, path: &str) -> HttpResponse {
    h2_request(addr, "GET", path, &addr.to_string(), &[], PROTOCOL_TIMEOUT).await
}

fn retained_stream_router() -> (
    Router,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Semaphore>,
) {
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
    (router, entered_rx, release)
}

async fn open_http2_client(
    addr: std::net::SocketAddr,
) -> (
    h2::client::SendRequest<Bytes>,
    tokio::task::JoinHandle<Result<(), h2::Error>>,
) {
    let tcp = bounded(
        tokio::net::TcpStream::connect(addr),
        PROTOCOL_TIMEOUT,
        "the HTTP/2 client connect",
    )
    .await
    .unwrap();
    let (client, connection) = bounded(
        h2::client::handshake(tcp),
        PROTOCOL_TIMEOUT,
        "the HTTP/2 handshake",
    )
    .await
    .unwrap();
    (client, tokio::spawn(connection))
}

async fn await_goaway(client: &mut h2::client::SendRequest<Bytes>) -> h2::Error {
    bounded(
        std::future::poll_fn(|context| match client.poll_ready(context) {
            std::task::Poll::Ready(Err(error)) => std::task::Poll::Ready(error),
            std::task::Poll::Ready(Ok(())) | std::task::Poll::Pending => {
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }),
        PROTOCOL_TIMEOUT,
        "the HTTP/2 GOAWAY",
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
            let answered = runtime_support::block_on(h2c_get(addr, "/hello"));
            assert_eq!(answered.status, 200);
            assert_eq!(answered.body.as_ref(), b"hi");
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
        PROTOCOL_TIMEOUT,
        "the HTTP/1.1 request",
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), "hi");

    let answered = h2c_get(addr, "/hello").await;
    assert_eq!(answered.status, 200);
    assert_eq!(answered.body.as_ref(), b"hi");
    runtime::request_shutdown();
}

#[camber::test]
async fn graceful_http2_sends_goaway_drains_stream_and_then_joins() {
    let (router, entered_rx, release) = retained_stream_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, router);
    let (mut client, connection) = open_http2_client(addr).await;

    let request = ::http::Request::get(format!("http://{addr}/retained"))
        .version(::http::Version::HTTP_2)
        .body(())
        .unwrap();
    client = bounded(client.ready(), PROTOCOL_TIMEOUT, "HTTP/2 client readiness")
        .await
        .unwrap();
    let (retained_response, _) = client.send_request(request, true).unwrap();
    bounded(entered_rx, PROTOCOL_TIMEOUT, "the HTTP/2 handler entry")
        .await
        .unwrap();

    runtime::request_shutdown();
    let new_stream_rejection = await_goaway(&mut client).await;
    assert!(new_stream_rejection.is_go_away());
    assert!(new_stream_rejection.is_remote());
    assert_eq!(new_stream_rejection.reason(), Some(h2::Reason::NO_ERROR));

    let mut completion = Box::pin(handle.into_future());
    if let Some(result) = completion.as_mut().now_or_never() {
        release.add_permits(1);
        panic!("ServerHandle completed while the accepted stream was retained: {result:?}");
    }

    release.add_permits(1);
    let response = bounded(
        retained_response,
        PROTOCOL_TIMEOUT,
        "the retained HTTP/2 response",
    )
    .await
    .expect("retained HTTP/2 stream was not drained");
    assert_eq!(response.status(), 200);
    let body_bytes = drain_h2_body(
        response.into_body(),
        "retained body frame",
        PROTOCOL_TIMEOUT,
    )
    .await;
    assert_eq!(body_bytes.as_ref(), b"drained");

    let result: Result<(), RuntimeError> =
        bounded(completion, PROTOCOL_TIMEOUT, "the HTTP/2 server join").await;
    assert!(result.is_ok(), "graceful ServerHandle result: {result:?}");
    bounded(connection, PROTOCOL_TIMEOUT, "the HTTP/2 connection join")
        .await
        .unwrap()
        .unwrap();
}
