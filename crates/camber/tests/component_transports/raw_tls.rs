use std::sync::Arc;

use crate::cohort_support::{bounded, join_server};
use crate::tls_support;

async fn tls_echo_handler(mut stream: camber::net::TlsStream) -> Result<(), camber::RuntimeError> {
    let mut buf = [0_u8; 1024];
    let count = stream.read(&mut buf).await?;
    stream.write_all(&buf[..count]).await?;
    Ok(())
}

#[camber::test]
async fn tls_raw_echo_server() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let server_config = tls_support::build_server_config(&cert_pem, &key_pem);
    let client_config = Arc::new(tls_support::tls_client_config(&[&cert_pem]));
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_tls_listener(listener, server_config, tls_echo_handler).await
    });

    let response = bounded(async {
        let mut stream = camber::tls::connect_with(&addr.to_string(), "localhost", client_config)
            .await
            .unwrap();
        stream.write_all(b"hello").await.unwrap();
        let mut buf = [0_u8; 1024];
        let count = stream.read(&mut buf).await.unwrap();
        stream.shutdown().await.unwrap();
        buf[..count].to_vec()
    })
    .await;
    assert_eq!(response, b"hello");

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn tls_connect_rejects_invalid_cert() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let server_config = tls_support::build_server_config(&cert_pem, &key_pem);
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_tls_listener(listener, server_config, tls_echo_handler).await
    });

    let result = bounded(camber::tls::connect(&addr.to_string(), "localhost")).await;
    assert!(matches!(result, Err(camber::RuntimeError::Tls(_))));

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn tls_peer_certificates_available() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let server_config = tls_support::build_server_config(&cert_pem, &key_pem);
    let client_config = Arc::new(tls_support::tls_client_config(&[&cert_pem]));
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_tls_listener(listener, server_config, tls_echo_handler).await
    });

    let stream = bounded(camber::tls::connect_with(
        &addr.to_string(),
        "localhost",
        client_config,
    ))
    .await
    .unwrap();
    let peer_certs = stream
        .peer_certificates()
        .expect("peer certificates should be available");
    assert!(
        !peer_certs.is_empty(),
        "peer certificates should not be empty"
    );

    camber::runtime::request_shutdown();
    join_server(handle).await;
}
