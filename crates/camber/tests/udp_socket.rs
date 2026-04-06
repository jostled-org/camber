use std::time::Duration;

#[camber::test]
async fn udp_echo_loopback() {
    let socket_a = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let socket_b = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let addr_b = socket_b.local_addr().unwrap();
    let addr_a = socket_a.local_addr().unwrap();

    // A sends "hello" to B
    let n = socket_a
        .send_to(b"hello", &addr_b.to_string())
        .await
        .unwrap();
    assert_eq!(n, 5);

    // B receives, asserts payload and sender address
    let mut buf = [0u8; 64];
    let (n, sender) = socket_b.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(sender, addr_a);

    // B sends "world" back to A
    let n = socket_b
        .send_to(b"world", &addr_a.to_string())
        .await
        .unwrap();
    assert_eq!(n, 5);

    // A receives, asserts payload
    let (n, _) = socket_a.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"world");
}

#[camber::test]
async fn udp_connected_mode() {
    let socket_a = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let socket_b = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let addr_a = socket_a.local_addr().unwrap();
    let addr_b = socket_b.local_addr().unwrap();

    // A connects to B
    socket_a.connect(&addr_b.to_string()).await.unwrap();

    // A sends via send() (no address)
    socket_a.send(b"connected").await.unwrap();

    // B receives via recv_from(), asserts payload
    let mut buf = [0u8; 64];
    let (n, sender) = socket_b.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"connected");
    assert_eq!(sender, addr_a);

    // B connects to A, sends via send()
    socket_b.connect(&addr_a.to_string()).await.unwrap();
    socket_b.send(b"reply").await.unwrap();

    // A receives via recv()
    let n = socket_a.recv(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"reply");
}

#[camber::test]
async fn udp_timeout_on_recv() {
    let socket = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut buf = [0u8; 64];

    let result = camber::timeout(Duration::from_millis(50), socket.recv_from(&mut buf)).await;
    assert!(matches!(result, Err(camber::RuntimeError::Timeout)));
}

#[camber::test]
async fn udp_echo_server_via_serve() {
    let listener = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_udp_on(listener, |datagram, src, socket| async move {
            socket.send_to(&datagram, &src.to_string()).await?;
            Ok(())
        })
        .await
    });

    tokio::task::yield_now().await;

    let client = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"hello", &addr.to_string()).await.unwrap();

    let mut buf = [0u8; 64];
    let (n, sender) = client.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello");
    assert_eq!(sender, addr);

    camber::runtime::request_shutdown();
    handle.await.unwrap().unwrap();
}

#[camber::test]
async fn udp_server_handles_multiple_clients() {
    let listener = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_udp_on(listener, |datagram, src, socket| async move {
            socket.send_to(&datagram, &src.to_string()).await?;
            Ok(())
        })
        .await
    });

    tokio::task::yield_now().await;

    let mut join_handles = Vec::new();
    for i in 0..5u8 {
        let jh = tokio::spawn(async move {
            let client = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let payload = [i; 8];
            client.send_to(&payload, &addr.to_string()).await.unwrap();

            let mut buf = [0u8; 64];
            let (n, _) = client.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], &payload);
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
async fn udp_server_stops_on_shutdown() {
    let listener = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = camber::spawn_async(async move {
        camber::net::serve_udp_on(listener, |datagram, src, socket| async move {
            socket.send_to(&datagram, &src.to_string()).await?;
            Ok(())
        })
        .await
    });

    tokio::task::yield_now().await;

    // Verify echo works
    let client = camber::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(b"ping", &addr.to_string()).await.unwrap();

    let mut buf = [0u8; 64];
    let (n, _) = client.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");
    drop(client);

    camber::runtime::request_shutdown();
    let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    result.unwrap().unwrap().unwrap();
}
