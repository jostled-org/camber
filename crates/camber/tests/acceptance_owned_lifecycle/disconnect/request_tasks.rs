//! Request task admission must retain the server's runtime across HTTP versions.

use super::fixture::{BOUND, DRIVER_ONLY, bounded, with_drain_window};
use super::peer::send;
use super::routes::probe_router;
use super::servers::with_owned_server;
use crate::common::{
    HttpResponse, PersistentH2Client, drain_h2_body, generate_self_signed_cert, server_tls_config,
    tls_client_config,
};
use camber::http::{Request, Response, Router};
use camber::runtime;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use tokio::sync::Semaphore;

/// Both public task forms run after the request future has suspended.
pub(super) async fn task_results(value: usize, ran: Arc<AtomicUsize>) -> String {
    tokio::task::yield_now().await;
    let blocking_ran = Arc::clone(&ran);
    let blocking = camber::spawn(move || {
        blocking_ran.fetch_add(1, Ordering::SeqCst);
        value
    })
    .await;
    let asynchronous = camber::spawn_async(async move {
        tokio::task::yield_now().await;
        ran.fetch_add(1, Ordering::SeqCst);
        value + 1
    })
    .await;
    format!("{blocking:?}/{asynchronous:?}")
}

pub(super) fn task_router() -> Router {
    counted_router(Arc::new(AtomicUsize::new(0)))
}

fn counted_router(ran: Arc<AtomicUsize>) -> Router {
    let mut router = probe_router();
    router.get("/tasks", move |request: &Request| {
        let ran = Arc::clone(&ran);
        let value = request
            .header("x-value")
            .and_then(|value| value.parse().ok())
            .unwrap_or(40);
        async move { Response::text(200, &task_results(value, ran).await) }
    });
    router
}

pub(super) async fn h2_answer(addr: SocketAddr) -> HttpResponse {
    let mut client = PersistentH2Client::connect(addr, BOUND).await;
    let mut stream = client.open_paced("GET", "/tasks", "localhost", &[]).await;
    stream.finish();
    let response = stream.answer().await;
    drop(stream);
    client.close().await;
    response
}

pub(super) fn assert_tasks(response: HttpResponse, expected: &str) {
    assert_task_answer(response.status, &response.body, expected);
}

fn assert_task_answer(status: u16, body: &[u8], expected: &str) {
    assert_eq!(status, 200);
    assert_eq!(String::from_utf8_lossy(body), expected);
}

/// One HTTP/2 exchange over TLS, where ALPN rather than a cleartext preface
/// selects the protocol.
async fn tls_h2_answer(
    addr: SocketAddr,
    connector: &tokio_rustls::TlsConnector,
) -> (u16, Box<[u8]>) {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(name, tcp).await.unwrap();
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(b"h2".as_slice()),
        "ALPN did not select HTTP/2"
    );
    let (sender, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    let mut sender = sender.ready().await.unwrap();
    let request = ::http::Request::get("https://localhost/tasks")
        .body(())
        .unwrap();
    let (response, _) = sender.send_request(request, true).unwrap();
    let response = response.await.unwrap();
    let status = response.status().as_u16();
    let body = drain_h2_body(response.into_body(), "the TLS HTTP/2 task body", BOUND).await;
    drop(sender);
    driver.abort();
    match driver.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("the TLS HTTP/2 driver failed: {error}"),
        Err(error) => assert!(
            error.is_cancelled(),
            "the TLS HTTP/2 driver join failed: {error}"
        ),
    }
    (status, body)
}

#[test]
fn request_tasks_admit_both_forms_after_suspension_on_http1_and_http2() {
    with_owned_server(task_router(), |addr| {
        assert_tasks(send(addr, "GET", "/tasks", "HTTP/1 tasks"), "Ok(40)/Ok(41)");
        assert_tasks(bounded("HTTP/2 tasks", h2_answer(addr)), "Ok(40)/Ok(41)");
    });
}

#[test]
fn request_tasks_admit_both_forms_over_tls_alpn_http2() {
    let (cert, key) = generate_self_signed_cert();
    let tls_config = server_tls_config(&cert, &key);
    let mut client_config = tls_client_config(&[&cert]);
    client_config.alpn_protocols = vec![b"h2".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    runtime::builder()
        .worker_threads(4)
        .run(move || {
            let listener = bounded(
                "the TLS listener bind",
                tokio::net::TcpListener::bind("127.0.0.1:0"),
            )
            .unwrap();
            let addr = listener.local_addr().unwrap();
            let handle =
                camber::http::serve_background_tls(listener, task_router(), tls_config).unwrap();
            let (status, body) = bounded("TLS HTTP/2 tasks", tls_h2_answer(addr, &connector));
            assert_task_answer(status, &body, "Ok(40)/Ok(41)");
            drop(handle);
        })
        .unwrap();
}

#[test]
fn request_tasks_concurrent_http2_streams_keep_their_values() {
    let (entered, entries) = mpsc::channel();
    let releases = [Arc::new(Semaphore::new(0)), Arc::new(Semaphore::new(0))];
    with_owned_server(concurrent_router(&releases, entered), |addr| {
        bounded("concurrent HTTP/2 tasks", async {
            let mut client = PersistentH2Client::connect(addr, BOUND).await;
            let mut first = client
                .open_paced("GET", "/tasks", "localhost", &[("x-value", "7")])
                .await;
            let mut second = client
                .open_paced("GET", "/tasks", "localhost", &[("x-value", "90")])
                .await;
            first.finish();
            second.finish();
            let mut values = [
                entries.recv_timeout(BOUND).unwrap(),
                entries.recv_timeout(BOUND).unwrap(),
            ];
            values.sort();
            assert_eq!(values, [7, 90]);
            releases[1].add_permits(1);
            assert_tasks(second.answer().await, "Ok(90)/Ok(91)");
            releases[0].add_permits(1);
            assert_tasks(first.answer().await, "Ok(7)/Ok(8)");
            drop((first, second));
            client.close().await;
        });
    });
}

fn concurrent_router(releases: &[Arc<Semaphore>; 2], entered: mpsc::Sender<usize>) -> Router {
    let [first, second] = releases.each_ref().map(Arc::clone);
    let mut router = probe_router();
    router.get("/tasks", move |req: &Request| {
        let value = req.header("x-value").unwrap().parse().unwrap();
        let release = Arc::clone(match value {
            7 => &first,
            _ => &second,
        });
        let entered = entered.clone();
        async move {
            entered.send(value).unwrap();
            release.acquire().await.unwrap().forget();
            Response::text(
                200,
                &task_results(value, Arc::new(AtomicUsize::new(0))).await,
            )
        }
    });
    router
}

#[test]
fn request_tasks_refuse_closed_owner_on_http1_and_http2() {
    let ran = Arc::new(AtomicUsize::new(0));
    let observed = with_drain_window(
        None,
        counted_router(Arc::clone(&ran)),
        |_| (),
        DRIVER_ONLY,
        |addr, ()| {
            assert_tasks(
                send(addr, "GET", "/tasks", "closed HTTP/1 tasks"),
                "Err(ScopeClosed)/Err(ScopeClosed)",
            );
            assert_tasks(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(h2_answer(addr)),
                "Err(ScopeClosed)/Err(ScopeClosed)",
            );
        },
    );
    assert!(
        observed.probed.is_some(),
        "the drain window with a closed owner was never observed"
    );
    assert!(
        observed.reached_zero,
        "the closed owner's scope never drained"
    );
    assert_eq!(ran.load(Ordering::SeqCst), 0, "a refused task body ran");
}

#[tokio::test]
async fn request_tasks_bare_tokio_does_not_create_camber_authority() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, task_router()).unwrap();
    assert_tasks(h2_answer(addr).await, "Err(NoRuntime)/Err(NoRuntime)");
    let response =
        tokio::task::spawn_blocking(move || send(addr, "GET", "/tasks", "standalone HTTP/1 tasks"))
            .await
            .unwrap();
    assert_tasks(response, "Err(NoRuntime)/Err(NoRuntime)");
    handle.shutdown_and_join().await.unwrap();
}

#[test]
fn request_tasks_captured_absence_masks_foreign_thread_runtime() {
    runtime::builder()
        .worker_threads(4)
        .run(|| {
            let (addr, server) = bounded(
                "standalone server construction",
                tokio::spawn(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let addr = listener.local_addr().unwrap();
                    (
                        addr,
                        camber::http::serve_async(listener, task_router()).unwrap(),
                    )
                }),
            )
            .unwrap();
            assert_eq!(camber::spawn(|| 1).join().unwrap(), 1);
            tokio::task::block_in_place(|| {
                let executor = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                executor.block_on(async {
                    let serving = tokio::spawn(server);
                    assert_tasks(h2_answer(addr).await, "Err(NoRuntime)/Err(NoRuntime)");
                    let response = tokio::task::spawn_blocking(move || {
                        send(addr, "GET", "/tasks", "captured standalone HTTP/1 tasks")
                    })
                    .await
                    .unwrap();
                    assert_tasks(response, "Err(NoRuntime)/Err(NoRuntime)");
                    serving.abort();
                    assert!(serving.await.unwrap_err().is_cancelled());
                });
            });
            assert_eq!(camber::spawn(|| 2).join().unwrap(), 2);
        })
        .unwrap();
}
