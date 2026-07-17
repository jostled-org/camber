mod common;

use camber::RuntimeError;
use camber::http::{HostRouter, Request, Response, Router};
use futures_util::FutureExt;
use std::future::IntoFuture;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

struct RetainedRequest {
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
    ownership: Arc<()>,
    response: &'static str,
}

fn retained_router(
    response: &'static str,
) -> (
    Router,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    Weak<()>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let ownership = Arc::new(());
    let ownership_probe = Arc::downgrade(&ownership);
    let retained = Mutex::new(Some(RetainedRequest {
        entered: entered_tx,
        release: release_rx,
        ownership,
        response,
    }));

    let mut router = Router::new();
    router.get("/retained", move |_req: &Request| {
        let retained = retained
            .lock()
            .unwrap()
            .take()
            .expect("retained route called more than once");
        async move {
            retained.entered.send(()).unwrap();
            let _ = retained.release.await;
            drop(retained.ownership);
            Response::text(200, retained.response)
        }
    });

    (router, entered_rx, release_tx, ownership_probe)
}

fn host_dispatch(router: Router) -> HostRouter {
    let mut host_router = HostRouter::new();
    host_router.set_default(router);
    host_router
}

async fn http_get(addr: std::net::SocketAddr, host: Option<&str>, path: &str) -> (u16, String) {
    let request = reqwest::Client::new().get(format!("http://{addr}{path}"));
    let request = match host {
        Some(host) => request.header("host", host),
        None => request,
    };
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let body = response.text().await.unwrap();
    (status, body)
}

async fn wait_for_admission_to_stop(addr: std::net::SocketAddr) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(stream) => drop(stream),
                Err(_) => return,
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server continued accepting after graceful shutdown");
}

fn assert_flat_ok(result: Result<(), RuntimeError>, variant: &str) {
    assert!(result.is_ok(), "{variant} returned {result:?}");
}

/// Make an HTTPS GET request using hyper over TLS, returning (status, body).
async fn https_get(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
) -> (u16, String) {
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, tcp).await.unwrap();

    let io = hyper_util::rt::TokioIo::new(tls_stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await.unwrap();
    tokio::spawn(conn);

    let req = hyper::Request::get(format!("http://localhost{path}"))
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();

    use http_body_util::BodyExt;
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    (status, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_tls_accepts_https_connection() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert_pem, &key_pem);

    let mut router = Router::new();
    router.get("/tls-hello", |_req: &Request| async {
        Response::text(200, "tls works")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async_tls(listener, router, tls_config));

    let client_config = common::tls_client_config(&[&cert_pem]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let (status, body) = https_get(&connector, addr, "/tls-hello").await;

    assert_eq!(status, 200);
    assert_eq!(body, "tls works");
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_async_hosts_dispatches_by_host() {
    let mut router_a = Router::new();
    router_a.get("/who", |_req: &Request| async {
        Response::text(200, "host-a")
    });

    let mut router_b = Router::new();
    router_b.get("/who", |_req: &Request| async {
        Response::text(200, "host-b")
    });

    let mut host_router = HostRouter::new();
    host_router.add("a.test", router_a);
    host_router.add("b.test", router_b);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async_hosts(listener, host_router));

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Request with Host: a.test
    let resp_a = reqwest::Client::new()
        .get(format!("http://{addr}/who"))
        .header("host", "a.test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp_a.status(), 200);
    assert_eq!(resp_a.text().await.unwrap(), "host-a");

    // Request with Host: b.test
    let resp_b = reqwest::Client::new()
        .get(format!("http://{addr}/who"))
        .header("host", "b.test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp_b.status(), 200);
    assert_eq!(resp_b.text().await.unwrap(), "host-b");
}

#[camber::test]
async fn serve_background_tls_runs_in_background() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert_pem, &key_pem);

    let mut router = Router::new();
    router.get("/bg-tls", |_req: &Request| async {
        Response::text(200, "background tls")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background_tls(listener, router, tls_config);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let client_config = common::tls_client_config(&[&cert_pem]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let (status, body) = https_get(&connector, addr, "/bg-tls").await;

    assert_eq!(status, 200);
    assert_eq!(body, "background tls");

    handle.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // After cancellation, new connections should fail
    let tcp_result = tokio::time::timeout(
        Duration::from_millis(500),
        tokio::net::TcpStream::connect(addr),
    )
    .await;
    match tcp_result {
        Ok(Ok(tcp)) => {
            let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let tls_result = connector.connect(server_name, tcp).await;
            assert!(
                tls_result.is_err(),
                "TLS handshake should fail after cancel"
            );
        }
        Ok(Err(_)) => {} // Connection refused — expected
        Err(_) => {}     // Timeout — expected
    }
}

/// 1.T3: Background server handle exposes flat Result<(), RuntimeError>,
/// not nested Result<Result<(), RuntimeError>, JoinError>.
#[camber::test]
async fn serve_background_handle_exposes_flat_error() {
    let mut router = Router::new();
    router.get("/flat", |_req: &Request| async {
        Response::text(200, "flat")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = camber::http::serve_background(listener, router);

    handle.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The key assertion: `.await` returns Result<(), RuntimeError> directly,
    // not Result<Result<(), RuntimeError>, _>. If the type were nested,
    // this line would not compile.
    let result: Result<(), RuntimeError> = handle.await;
    assert!(
        result.is_err(),
        "expected Err(Cancelled) after cancel, got Ok"
    );
}

/// 1.T8: Every background constructor stops admission, drains retained work,
/// releases request ownership, and exposes the same flat successful result.
#[camber::test]
async fn all_background_variants_share_internal_lifecycle_behavior() {
    let (plain_router, plain_entered, plain_release, plain_ownership) = retained_router("plain");
    let (tls_router, tls_entered, tls_release, tls_ownership) = retained_router("tls");
    let (host_router, host_entered, host_release, host_ownership) = retained_router("host");
    let (host_tls_router, host_tls_entered, host_tls_release, host_tls_ownership) =
        retained_router("host-tls");

    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert_pem, &key_pem);
    let client_config = common::tls_client_config(&[&cert_pem]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

    let plain_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let plain_addr = plain_listener.local_addr().unwrap();
    let plain_handle = camber::http::serve_background(plain_listener, plain_router);

    let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_addr = tls_listener.local_addr().unwrap();
    let tls_handle =
        camber::http::serve_background_tls(tls_listener, tls_router, Arc::clone(&tls_config));

    let host_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host_addr = host_listener.local_addr().unwrap();
    let host_handle =
        camber::http::serve_background_hosts(host_listener, host_dispatch(host_router));

    let host_tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host_tls_addr = host_tls_listener.local_addr().unwrap();
    let host_tls_handle = camber::http::serve_background_hosts_tls(
        host_tls_listener,
        host_dispatch(host_tls_router),
        tls_config,
    );

    let plain_client = tokio::spawn(http_get(plain_addr, None, "/retained"));
    let tls_connector = connector.clone();
    let tls_client =
        tokio::spawn(async move { https_get(&tls_connector, tls_addr, "/retained").await });
    let host_client = tokio::spawn(http_get(host_addr, Some("localhost"), "/retained"));
    let host_tls_client =
        tokio::spawn(async move { https_get(&connector, host_tls_addr, "/retained").await });

    tokio::time::timeout(Duration::from_secs(5), async {
        plain_entered.await.unwrap();
        tls_entered.await.unwrap();
        host_entered.await.unwrap();
        host_tls_entered.await.unwrap();
    })
    .await
    .expect("not every background variant dispatched its retained request");

    assert!(plain_ownership.upgrade().is_some());
    assert!(tls_ownership.upgrade().is_some());
    assert!(host_ownership.upgrade().is_some());
    assert!(host_tls_ownership.upgrade().is_some());

    let mut plain_join = Box::pin(plain_handle.into_future());
    let mut tls_join = Box::pin(tls_handle.into_future());
    let mut host_join = Box::pin(host_handle.into_future());
    let mut host_tls_join = Box::pin(host_tls_handle.into_future());

    camber::runtime::request_shutdown();
    tokio::join!(
        wait_for_admission_to_stop(plain_addr),
        wait_for_admission_to_stop(tls_addr),
        wait_for_admission_to_stop(host_addr),
        wait_for_admission_to_stop(host_tls_addr),
    );

    assert!(
        plain_join.as_mut().now_or_never().is_none(),
        "plain handle completed while request ownership was retained"
    );
    assert!(
        tls_join.as_mut().now_or_never().is_none(),
        "TLS handle completed while request ownership was retained"
    );
    assert!(
        host_join.as_mut().now_or_never().is_none(),
        "host handle completed while request ownership was retained"
    );
    assert!(
        host_tls_join.as_mut().now_or_never().is_none(),
        "host-plus-TLS handle completed while request ownership was retained"
    );

    plain_release.send(()).unwrap();
    tls_release.send(()).unwrap();
    host_release.send(()).unwrap();
    host_tls_release.send(()).unwrap();

    let responses = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(plain_client, tls_client, host_client, host_tls_client)
    })
    .await
    .expect("retained requests did not complete after release");
    assert_eq!(responses.0.unwrap(), (200, "plain".to_owned()));
    assert_eq!(responses.1.unwrap(), (200, "tls".to_owned()));
    assert_eq!(responses.2.unwrap(), (200, "host".to_owned()));
    assert_eq!(responses.3.unwrap(), (200, "host-tls".to_owned()));

    let results = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(plain_join, tls_join, host_join, host_tls_join)
    })
    .await
    .expect("background variants did not join after retained requests completed");
    assert_flat_ok(results.0, "plain");
    assert_flat_ok(results.1, "TLS");
    assert_flat_ok(results.2, "host");
    assert_flat_ok(results.3, "host-plus-TLS");

    assert!(plain_ownership.upgrade().is_none());
    assert!(tls_ownership.upgrade().is_none());
    assert!(host_ownership.upgrade().is_none());
    assert!(host_tls_ownership.upgrade().is_none());
}
