mod common;

use camber::RuntimeError;
use camber::http::{HostRouter, Request, Response, Router};
use std::sync::Arc;
use std::time::Duration;

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
