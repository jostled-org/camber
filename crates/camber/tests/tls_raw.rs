mod common;

use std::sync::Arc;

async fn tls_echo_handler(mut stream: camber::net::TlsStream) -> Result<(), camber::RuntimeError> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    stream.write_all(&buf[..n]).await?;
    Ok(())
}

#[camber::test]
async fn tls_raw_echo_server() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let server_config = common::build_server_config(&cert_pem, &key_pem);
    let client_config = Arc::new(common::tls_client_config(&[&cert_pem]));

    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_tls_listener(listener, server_config, tls_echo_handler).await
    });

    tokio::task::yield_now().await;

    let mut stream = camber::tls::connect_with(&addr.to_string(), "localhost", client_config)
        .await
        .unwrap();
    stream.write_all(b"hello").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");

    stream.shutdown().await.unwrap();

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}

#[camber::test]
async fn tls_connect_rejects_invalid_cert() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let server_config = common::build_server_config(&cert_pem, &key_pem);

    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_tls_listener(listener, server_config, tls_echo_handler).await
    });

    tokio::task::yield_now().await;

    // Connect without trusting the self-signed cert — should fail
    let result = camber::tls::connect(&addr.to_string(), "localhost").await;
    assert!(result.is_err());
    match result.unwrap_err() {
        camber::RuntimeError::Tls(_) => {}
        other => panic!("expected RuntimeError::Tls, got: {other:?}"),
    }

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}

#[camber::test]
async fn tls_peer_certificates_available() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let server_config = common::build_server_config(&cert_pem, &key_pem);
    let client_config = Arc::new(common::tls_client_config(&[&cert_pem]));

    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_tls_listener(listener, server_config, tls_echo_handler).await
    });

    tokio::task::yield_now().await;

    let stream = camber::tls::connect_with(&addr.to_string(), "localhost", client_config)
        .await
        .unwrap();

    let peer_certs = stream.peer_certificates();
    assert!(
        peer_certs.is_some(),
        "peer certificates should be available"
    );
    assert!(
        !peer_certs.unwrap().is_empty(),
        "peer certificates should not be empty"
    );

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}
