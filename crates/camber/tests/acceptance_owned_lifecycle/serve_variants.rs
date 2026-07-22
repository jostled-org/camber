use crate::common;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, lifecycle};
use camber::http::{HostRouter, Request, Response, Router};
use futures_util::FutureExt;
use std::future::IntoFuture;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

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

async fn http_get(
    addr: std::net::SocketAddr,
    host: Option<&str>,
    path: &str,
) -> Result<(u16, String), reqwest::Error> {
    let request = reqwest::Client::new().get(format!("http://{addr}{path}"));
    let request = match host {
        Some(host) => request.header("host", host),
        None => request,
    };
    let response = request.send().await?;
    let status = response.status().as_u16();
    let body = response.text().await?;
    Ok((status, body))
}

async fn wait_for_checkpoint(controller: &LifecycleController, checkpoint: LifecycleCheckpoint) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap()
        .unwrap();
}

fn assert_flat_ok(result: Result<(), RuntimeError>, variant: &str) {
    assert!(result.is_ok(), "{variant} returned {result:?}");
}

/// Make an HTTPS GET request using hyper over TLS, returning (status, body).
async fn https_get(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
) -> Result<(u16, String), Box<str>> {
    let req = hyper::Request::get(format!("http://localhost{path}"))
        .header("connection", "close")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .map_err(|error| format!("HTTP request build failed: {error}").into_boxed_str())?;
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|error| format!("TLS TCP connect failed: {error}").into_boxed_str())?;
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|error| format!("invalid TLS server name: {error}").into_boxed_str())?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|error| format!("TLS handshake failed: {error}").into_boxed_str())?;

    let io = hyper_util::rt::TokioIo::new(tls_stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|error| format!("HTTP handshake failed: {error}").into_boxed_str())?;
    let connection = tokio::spawn(conn);
    let exchange = async {
        let resp = sender
            .send_request(req)
            .await
            .map_err(|error| format!("HTTPS request failed: {error}").into_boxed_str())?;
        let status = resp.status().as_u16();
        use http_body_util::BodyExt;
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("HTTPS body failed: {error}").into_boxed_str())?
            .to_bytes();
        let body = String::from_utf8(body.to_vec())
            .map_err(|error| format!("HTTPS body was not UTF-8: {error}").into_boxed_str())?;
        Ok::<_, Box<str>>((status, body))
    }
    .await;
    drop(sender);
    let driver = tokio::time::timeout(EVENT_TIMEOUT, connection)
        .await
        .map_err(|error| format!("HTTP connection join timed out: {error}").into_boxed_str())?
        .map_err(|error| format!("HTTP connection task failed: {error}").into_boxed_str())?
        .map_err(|error| format!("HTTP connection failed: {error}").into_boxed_str());
    match (exchange, driver) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
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
    let server = tokio::spawn(camber::http::serve_async_tls(listener, router, tls_config));

    let client_config = common::tls_client_config(&[&cert_pem]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let response = https_get(&connector, addr, "/tls-hello").await;

    server.abort();
    let join = tokio::time::timeout(EVENT_TIMEOUT, server).await.unwrap();
    assert!(join.unwrap_err().is_cancelled());

    let (status, body) = response.unwrap();
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
    let server = tokio::spawn(camber::http::serve_async_hosts(listener, host_router));

    // Request with Host: a.test
    let response_a = http_get(addr, Some("a.test"), "/who").await;

    // Request with Host: b.test
    let response_b = http_get(addr, Some("b.test"), "/who").await;

    server.abort();
    let join = tokio::time::timeout(EVENT_TIMEOUT, server).await.unwrap();
    assert!(join.unwrap_err().is_cancelled());

    assert_eq!(response_a.unwrap(), (200, "host-a".to_owned()));
    assert_eq!(response_b.unwrap(), (200, "host-b".to_owned()));
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

    let client_config = common::tls_client_config(&[&cert_pem]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let response = https_get(&connector, addr, "/bg-tls").await;

    handle.cancel();
    let server_result = tokio::time::timeout(EVENT_TIMEOUT, handle).await.unwrap();
    assert!(matches!(server_result, Err(RuntimeError::Cancelled)));

    let (status, body) = response.unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, "background tls");

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

    // The key assertion: `.await` returns Result<(), RuntimeError> directly,
    // not Result<Result<(), RuntimeError>, _>. If the type were nested,
    // this line would not compile.
    let result: Result<(), RuntimeError> =
        tokio::time::timeout(EVENT_TIMEOUT, handle).await.unwrap();
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
    let plain_lifecycle = lifecycle(plain_addr).unwrap();
    plain_lifecycle
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    let plain_handle = camber::http::serve_background(plain_listener, plain_router);

    let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_addr = tls_listener.local_addr().unwrap();
    let tls_lifecycle = lifecycle(tls_addr).unwrap();
    tls_lifecycle
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    let tls_handle =
        camber::http::serve_background_tls(tls_listener, tls_router, Arc::clone(&tls_config));

    let host_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host_addr = host_listener.local_addr().unwrap();
    let host_lifecycle = lifecycle(host_addr).unwrap();
    host_lifecycle
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    let host_handle =
        camber::http::serve_background_hosts(host_listener, host_dispatch(host_router));

    let host_tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host_tls_addr = host_tls_listener.local_addr().unwrap();
    let host_tls_lifecycle = lifecycle(host_tls_addr).unwrap();
    host_tls_lifecycle
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
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
        wait_for_checkpoint(
            &plain_lifecycle,
            LifecycleCheckpoint::SupervisorSelectedRuntime
        ),
        wait_for_checkpoint(
            &tls_lifecycle,
            LifecycleCheckpoint::SupervisorSelectedRuntime
        ),
        wait_for_checkpoint(
            &host_lifecycle,
            LifecycleCheckpoint::SupervisorSelectedRuntime
        ),
        wait_for_checkpoint(
            &host_tls_lifecycle,
            LifecycleCheckpoint::SupervisorSelectedRuntime
        ),
    );
    for controller in [
        &plain_lifecycle,
        &tls_lifecycle,
        &host_lifecycle,
        &host_tls_lifecycle,
    ] {
        controller
            .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
            .unwrap();
        controller
            .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
            .unwrap();
    }
    tokio::join!(
        wait_for_checkpoint(
            &plain_lifecycle,
            LifecycleCheckpoint::BeforeSupervisorSelect
        ),
        wait_for_checkpoint(&tls_lifecycle, LifecycleCheckpoint::BeforeSupervisorSelect),
        wait_for_checkpoint(&host_lifecycle, LifecycleCheckpoint::BeforeSupervisorSelect),
        wait_for_checkpoint(
            &host_tls_lifecycle,
            LifecycleCheckpoint::BeforeSupervisorSelect
        ),
    );
    tokio::join!(
        common::assert_admission_closed(plain_addr, EVENT_TIMEOUT),
        common::assert_admission_closed(tls_addr, EVENT_TIMEOUT),
        common::assert_admission_closed(host_addr, EVENT_TIMEOUT),
        common::assert_admission_closed(host_tls_addr, EVENT_TIMEOUT),
    );
    for controller in [
        &plain_lifecycle,
        &tls_lifecycle,
        &host_lifecycle,
        &host_tls_lifecycle,
    ] {
        controller
            .release(LifecycleCheckpoint::BeforeSupervisorSelect)
            .unwrap();
    }

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
    assert_eq!(responses.0.unwrap().unwrap(), (200, "plain".to_owned()));
    assert_eq!(responses.1.unwrap().unwrap(), (200, "tls".to_owned()));
    assert_eq!(responses.2.unwrap().unwrap(), (200, "host".to_owned()));
    assert_eq!(responses.3.unwrap().unwrap(), (200, "host-tls".to_owned()));

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
