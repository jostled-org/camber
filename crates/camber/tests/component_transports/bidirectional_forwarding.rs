use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cohort_support::{bounded, join_server};
use crate::tls_support;

async fn echo_loop(mut stream: camber::net::TcpStream) -> Result<(), camber::RuntimeError> {
    let mut buf = [0_u8; 1024];
    loop {
        match stream.read(&mut buf).await? {
            0 => return Ok(()),
            count => stream.write_all(&buf[..count]).await?,
        }
    }
}

#[camber::test]
async fn forward_copies_bidirectionally() {
    let echo_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let echo_addr = echo_listener.local_addr().unwrap().tcp().unwrap();
    let echo_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(echo_listener, echo_loop).await
    });

    let proxy_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap().tcp().unwrap();
    let proxy_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(proxy_listener, move |client| {
            let echo_addr = echo_addr.to_string();
            async move {
                let upstream = camber::net::TcpStream::connect(&echo_addr).await?;
                camber::net::forward(client, upstream).await?;
                Ok(())
            }
        })
        .await
    });

    bounded(async {
        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        let mut buf = [0_u8; 1024];
        client.write_all(b"hello").await.unwrap();
        let count = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..count], b"hello");
        client.write_all(b"world").await.unwrap();
        let count = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..count], b"world");
    })
    .await;

    camber::runtime::request_shutdown();
    join_server(echo_handle).await;
    join_server(proxy_handle).await;
}

#[camber::test]
async fn forward_terminates_when_client_closes() {
    let echo_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let echo_addr = echo_listener.local_addr().unwrap().tcp().unwrap();
    let echo_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(echo_listener, echo_loop).await
    });

    let (forward_done_tx, forward_done_rx) = tokio::sync::oneshot::channel();
    let forward_done_tx = Arc::new(Mutex::new(Some(forward_done_tx)));
    let proxy_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap().tcp().unwrap();
    let proxy_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(proxy_listener, move |client| {
            let echo_addr = echo_addr.to_string();
            let forward_done_tx = Arc::clone(&forward_done_tx);
            async move {
                let upstream = camber::net::TcpStream::connect(&echo_addr).await?;
                camber::net::forward(client, upstream).await?;
                let sender = forward_done_tx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take();
                if let Some(sender) = sender {
                    let _ = sender.send(());
                }
                Ok(())
            }
        })
        .await
    });

    bounded(async {
        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        let mut buf = [0_u8; 1024];
        let count = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..count], b"hello");
        client.shutdown().await.unwrap();
        drop(client);

        // The channel is the race proof; the deadline only bounds a broken protocol.
        forward_done_rx.await.unwrap();
    })
    .await;

    camber::runtime::request_shutdown();
    join_server(echo_handle).await;
    join_server(proxy_handle).await;
}

#[camber::test]
async fn forward_terminates_when_upstream_closes() {
    async fn one_shot(mut stream: camber::net::TcpStream) -> Result<(), camber::RuntimeError> {
        let mut buf = [0_u8; 1024];
        let count = stream.read(&mut buf).await?;
        stream.write_all(&buf[..count]).await?;
        stream.shutdown().await?;
        Ok(())
    }

    let server_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let server_addr = server_listener.local_addr().unwrap().tcp().unwrap();
    let server_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(server_listener, one_shot).await
    });
    let proxy_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap().tcp().unwrap();
    let proxy_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(proxy_listener, move |client| {
            let server_addr = server_addr.to_string();
            async move {
                let upstream = camber::net::TcpStream::connect(&server_addr).await?;
                camber::net::forward(client, upstream).await?;
                Ok(())
            }
        })
        .await
    });

    let response = bounded(async {
        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        response
    })
    .await;
    assert_eq!(response, b"ping");

    camber::runtime::request_shutdown();
    join_server(server_handle).await;
    join_server(proxy_handle).await;
}

#[camber::test]
async fn forward_works_with_tls_streams() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let server_config = tls_support::build_server_config(&cert_pem, &key_pem);
    let client_config = Arc::new(tls_support::tls_client_config(&[&cert_pem]));
    let echo_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let echo_addr = echo_listener.local_addr().unwrap().tcp().unwrap();
    let echo_handle = camber::spawn_async(async move {
        async fn tls_echo(mut stream: camber::net::TlsStream) -> Result<(), camber::RuntimeError> {
            let mut buf = [0_u8; 1024];
            loop {
                match stream.read(&mut buf).await? {
                    0 => return Ok(()),
                    count => stream.write_all(&buf[..count]).await?,
                }
            }
        }
        camber::net::serve_tcp_tls_listener(echo_listener, server_config, tls_echo).await
    });

    let proxy_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap().tcp().unwrap();
    let proxy_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(proxy_listener, move |client| {
            let echo_addr = echo_addr.to_string();
            let client_config = Arc::clone(&client_config);
            async move {
                let upstream =
                    camber::tls::connect_with(&echo_addr, "localhost", client_config).await?;
                camber::net::forward(client, upstream).await?;
                Ok(())
            }
        })
        .await
    });

    let response = bounded(async {
        let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"tls-bridge").await.unwrap();
        let mut response = [0_u8; 1024];
        let count = client.read(&mut response).await.unwrap();
        response[..count].to_vec()
    })
    .await;
    assert_eq!(response, b"tls-bridge");

    camber::runtime::request_shutdown();
    join_server(echo_handle).await;
    join_server(proxy_handle).await;
}
