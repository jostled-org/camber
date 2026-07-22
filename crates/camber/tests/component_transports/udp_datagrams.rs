use std::time::Duration;

use crate::cohort_support::{bounded, join_server};

#[camber::test]
async fn udp_echo_loopback() {
    let socket_a = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let socket_b = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr_a = socket_a.local_addr().unwrap();
    let addr_b = socket_b.local_addr().unwrap();

    let sent = socket_a
        .send_to(b"hello", &addr_b.to_string())
        .await
        .unwrap();
    assert_eq!(sent, 5);

    let mut buf = [0_u8; 64];
    let (count, sender) = bounded(socket_b.recv_from(&mut buf)).await.unwrap();
    assert_eq!(&buf[..count], b"hello");
    assert_eq!(sender, addr_a);

    let sent = socket_b
        .send_to(b"world", &addr_a.to_string())
        .await
        .unwrap();
    assert_eq!(sent, 5);
    let (count, _) = bounded(socket_a.recv_from(&mut buf)).await.unwrap();
    assert_eq!(&buf[..count], b"world");
}

#[camber::test]
async fn udp_connected_mode() {
    let socket_a = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let socket_b = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr_a = socket_a.local_addr().unwrap();
    let addr_b = socket_b.local_addr().unwrap();

    socket_a.connect(&addr_b.to_string()).await.unwrap();
    socket_a.send(b"connected").await.unwrap();
    let mut buf = [0_u8; 64];
    let (count, sender) = bounded(socket_b.recv_from(&mut buf)).await.unwrap();
    assert_eq!(&buf[..count], b"connected");
    assert_eq!(sender, addr_a);

    socket_b.connect(&addr_a.to_string()).await.unwrap();
    socket_b.send(b"reply").await.unwrap();
    let count = bounded(socket_a.recv(&mut buf)).await.unwrap();
    assert_eq!(&buf[..count], b"reply");
}

#[camber::test]
async fn udp_timeout_on_recv() {
    let socket = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0_u8; 64];
    let result = camber::timeout(Duration::from_millis(50), socket.recv_from(&mut buf)).await;
    assert!(matches!(result, Err(camber::RuntimeError::Timeout)));
}

fn spawn_echo_server(
    listener: camber::net::UdpSocket,
) -> camber::AsyncJoinHandle<Result<(), camber::RuntimeError>> {
    camber::spawn_async(async move {
        camber::net::serve_udp_on(listener, |datagram, src, socket| async move {
            socket.send_to(&datagram, &src.to_string()).await?;
            Ok(())
        })
        .await
    })
}

#[camber::test]
async fn udp_echo_server_via_serve() {
    let listener = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = spawn_echo_server(listener);

    let client = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"hello", &addr.to_string()).await.unwrap();
    let mut buf = [0_u8; 64];
    let (count, sender) = bounded(client.recv_from(&mut buf)).await.unwrap();
    assert_eq!(&buf[..count], b"hello");
    assert_eq!(sender, addr);

    camber::runtime::request_shutdown();
    join_server(handle).await;
}

#[camber::test]
async fn udp_server_handles_multiple_clients() {
    let listener = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = spawn_echo_server(listener);

    bounded(async {
        let clients = (0..5_u8).map(|byte| {
            tokio::spawn(async move {
                let client = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let payload = [byte; 8];
                client.send_to(&payload, &addr.to_string()).await.unwrap();
                let mut buf = [0_u8; 64];
                let (count, _) = client.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..count], &payload);
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
async fn udp_server_stops_on_shutdown() {
    let listener = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = spawn_echo_server(listener);

    let client = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"ping", &addr.to_string()).await.unwrap();
    let mut buf = [0_u8; 64];
    let (count, _) = bounded(client.recv_from(&mut buf)).await.unwrap();
    assert_eq!(&buf[..count], b"ping");
    drop(client);

    camber::runtime::request_shutdown();
    join_server(handle).await;
}
