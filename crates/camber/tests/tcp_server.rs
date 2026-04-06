use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn echo_handler(mut stream: camber::net::TcpStream) -> Result<(), camber::RuntimeError> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await?;
    stream.write_all(&buf[..n]).await?;
    Ok(())
}

#[camber::test]
async fn tcp_echo_server() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    tokio::task::yield_now().await;

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(b"hello").await.unwrap();
    client.shutdown().await.unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response, b"hello");

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}

#[camber::test]
async fn tcp_server_concurrent_connections() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    tokio::task::yield_now().await;

    let mut join_handles = Vec::new();
    for i in 0..10u8 {
        let jh = tokio::spawn(async move {
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let payload = [i; 8];
            client.write_all(&payload).await.unwrap();
            client.shutdown().await.unwrap();

            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, payload);
        });
        join_handles.push(jh);
    }

    for jh in join_handles {
        jh.await.unwrap();
    }

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}

#[camber::test]
async fn tcp_server_stops_on_shutdown() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    tokio::task::yield_now().await;

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(b"ping").await.unwrap();
    client.shutdown().await.unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response, b"ping");
    drop(client);

    camber::runtime::request_shutdown();
    let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    result.unwrap().unwrap().unwrap();
}

#[camber::test]
async fn tcp_accept_loop_handles_connections() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    tokio::task::yield_now().await;

    for _ in 0..3 {
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"test").await.unwrap();
        client.shutdown().await.unwrap();

        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"test");
    }

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}

#[camber::test]
async fn tcp_connect_outbound() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    tokio::task::yield_now().await;

    let mut stream = camber::net::TcpStream::connect(&addr.to_string())
        .await
        .unwrap();
    stream.write_all(b"outbound").await.unwrap();
    stream.shutdown().await.unwrap();

    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"outbound");

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}
