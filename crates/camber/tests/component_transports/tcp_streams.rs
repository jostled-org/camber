use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::cohort_support::{bounded, join_server};

async fn echo_handler(mut stream: camber::net::TcpStream) -> Result<(), camber::RuntimeError> {
    let mut buf = [0_u8; 1024];
    let count = stream.read(&mut buf).await?;
    stream.write_all(&buf[..count]).await?;
    Ok(())
}

#[camber::test]
async fn tcp_echo_server() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    let response = bounded(async {
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"hello").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        response
    })
    .await;
    assert_eq!(response, b"hello");

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn tcp_server_concurrent_connections() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    bounded(async {
        let clients = (0..10_u8).map(|byte| {
            tokio::spawn(async move {
                let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
                let payload = [byte; 8];
                client.write_all(&payload).await.unwrap();
                client.shutdown().await.unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, payload);
            })
        });
        for client in clients {
            client.await.unwrap();
        }
    })
    .await;

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn tcp_server_stops_on_shutdown() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    let response = bounded(async {
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        client.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        response
    })
    .await;
    assert_eq!(response, b"ping");

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn tcp_accept_loop_handles_connections() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    bounded(async {
        for _ in 0..3 {
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            client.write_all(b"test").await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, b"test");
        }
    })
    .await;

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn tcp_connect_outbound() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, echo_handler).await
    });

    let response = bounded(async {
        let mut stream = camber::net::TcpStream::connect(&addr.to_string())
            .await
            .unwrap();
        stream.write_all(b"outbound").await.unwrap();
        stream.shutdown().await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        response
    })
    .await;
    assert_eq!(response, b"outbound");

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

struct HandlerDropWitness(Arc<AtomicBool>);

impl Drop for HandlerDropWitness {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[camber::test]
async fn tcp_accept_loop_owns_handler_tasks() {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let handler_dropped = Arc::new(AtomicBool::new(false));
    let handler_entered = Arc::clone(&entered);
    let handler_drop = Arc::clone(&handler_dropped);
    let handle = camber::spawn_async(async move {
        camber::net::serve_tcp_listener(listener, move |_| {
            handler_entered.notify_one();
            let witness = HandlerDropWitness(Arc::clone(&handler_drop));
            async move {
                std::future::pending::<()>().await;
                drop(witness);
                Ok(())
            }
        })
        .await
    });
    let client = tokio::net::TcpStream::connect(addr).await.unwrap();
    bounded(entered.notified()).await;

    camber::runtime::request_shutdown();
    join_server(handle).await;

    assert!(
        handler_dropped.load(Ordering::Acquire),
        "the accept loop must cancel and join its connection handlers"
    );
    drop(client);
}
