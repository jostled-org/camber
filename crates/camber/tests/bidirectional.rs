mod common;

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Echo handler: reads bytes and writes them back in a loop until EOF.
async fn echo_loop(mut stream: camber::net::TcpStream) -> Result<(), camber::RuntimeError> {
    let mut buf = [0u8; 1024];
    loop {
        let n = stream.read(&mut buf).await?;
        match n {
            0 => return Ok(()),
            n => stream.write_all(&buf[..n]).await?,
        }
    }
}

#[camber::test]
async fn forward_copies_bidirectionally() {
    // Start an echo server
    let echo_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let echo_addr = echo_listener.local_addr().unwrap().tcp().unwrap();

    let echo_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(echo_listener, echo_loop).await
    });

    // Start a proxy that forwards between client and echo server
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

    tokio::task::yield_now().await;

    // Client: send "hello", read it back
    let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(b"hello").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");

    // Send "world", read it back
    client.write_all(b"world").await.unwrap();

    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"world");

    drop(client);
    camber::runtime::request_shutdown();
    echo_handle.await.unwrap().unwrap();
    proxy_handle.await.unwrap().unwrap();
}

#[camber::test]
async fn forward_terminates_when_client_closes() {
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

    tokio::task::yield_now().await;

    let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(b"hello").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");

    // Client shuts down write side then drops
    client.shutdown().await.unwrap();
    drop(client);

    // forward() should return without hanging
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    camber::runtime::request_shutdown();
    echo_handle.await.unwrap().unwrap();
    proxy_handle.await.unwrap().unwrap();
}

#[camber::test]
async fn forward_terminates_when_upstream_closes() {
    // Server that reads one message, responds, then closes
    async fn one_shot(mut stream: camber::net::TcpStream) -> Result<(), camber::RuntimeError> {
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).await?;
        stream.write_all(&buf[..n]).await?;
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

    tokio::task::yield_now().await;

    let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(b"ping").await.unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response, b"ping");

    camber::runtime::request_shutdown();
    server_handle.await.unwrap().unwrap();
    proxy_handle.await.unwrap().unwrap();
}

#[camber::test]
async fn forward_works_with_tls_streams() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let server_config = common::build_server_config(&cert_pem, &key_pem);
    let client_config = Arc::new(common::tls_client_config(&[&cert_pem]));

    // TLS echo server
    let echo_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let echo_addr = echo_listener.local_addr().unwrap().tcp().unwrap();

    let echo_handle = camber::spawn_async(async move {
        async fn tls_echo(mut stream: camber::net::TlsStream) -> Result<(), camber::RuntimeError> {
            let mut buf = [0u8; 1024];
            loop {
                let n = stream.read(&mut buf).await?;
                match n {
                    0 => return Ok(()),
                    n => stream.write_all(&buf[..n]).await?,
                }
            }
        }
        camber::net::serve_tcp_tls_listener(echo_listener, server_config, tls_echo).await
    });

    // Plain TCP proxy: accepts plain TCP, TLS-connects to echo server, forwards
    let proxy_listener = camber::net::listen("127.0.0.1:0").unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap().tcp().unwrap();

    let proxy_handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(proxy_listener, move |client| {
            let echo_addr = echo_addr.to_string();
            let cc = Arc::clone(&client_config);
            async move {
                let upstream = camber::tls::connect_with(&echo_addr, "localhost", cc).await?;
                camber::net::forward(client, upstream).await?;
                Ok(())
            }
        })
        .await
    });

    tokio::task::yield_now().await;

    // Plain TCP client connects to proxy
    let mut client = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    client.write_all(b"tls-bridge").await.unwrap();

    let mut buf = [0u8; 1024];
    let n = client.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"tls-bridge");

    drop(client);
    camber::runtime::request_shutdown();
    echo_handle.await.unwrap().unwrap();
    proxy_handle.await.unwrap().unwrap();
}
