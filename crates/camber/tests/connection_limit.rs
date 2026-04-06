mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[test]
fn connection_limit_zero_rejected() {
    let err = camber::runtime::builder()
        .connection_limit(0)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| Ok::<(), camber::RuntimeError>(()))
        .unwrap_err();

    match err {
        camber::RuntimeError::InvalidArgument(msg) => {
            assert_eq!(msg.as_ref(), "connection_limit must be at least 1");
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

fn send_request(stream: &mut TcpStream, path: &str) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
}

fn read_status(stream: &mut TcpStream) -> u16 {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).expect("read response");
    let text = String::from_utf8_lossy(&buf[..n]);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0)
}

#[test]
fn connection_limit_blocks_third_connection_until_slot_frees() {
    camber::runtime::builder()
        .connection_limit(2)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "ok")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            std::thread::sleep(Duration::from_millis(50));

            // Open two keep-alive connections and hold them open.
            let mut conn1 = TcpStream::connect(addr).expect("connect 1");
            conn1.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let req_keepalive = "GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
            conn1.write_all(req_keepalive.as_bytes()).unwrap();
            let s1 = read_status(&mut conn1);
            assert_eq!(s1, 200);

            let mut conn2 = TcpStream::connect(addr).expect("connect 2");
            conn2.set_read_timeout(Some(Duration::from_secs(5))).ok();
            conn2.write_all(req_keepalive.as_bytes()).unwrap();
            let s2 = read_status(&mut conn2);
            assert_eq!(s2, 200);

            // Third connection — should block because both slots are occupied.
            let mut conn3 = TcpStream::connect(addr).expect("connect 3");
            conn3
                .set_read_timeout(Some(Duration::from_millis(300)))
                .ok();
            send_request(&mut conn3, "/hello");
            let result = {
                let mut buf = [0u8; 1];
                conn3.read(&mut buf)
            };
            // Should time out because no permit is available.
            assert!(
                result.is_err(),
                "third connection should block while two slots are occupied"
            );

            // Free a slot by closing the first connection.
            drop(conn1);
            std::thread::sleep(Duration::from_millis(200));

            // Now the third connection should complete.
            conn3.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let s3 = read_status(&mut conn3);
            assert_eq!(s3, 200);

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn connection_limit_releases_slot_after_connection_exit() {
    camber::runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "ok")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            std::thread::sleep(Duration::from_millis(50));

            // Open one connection, complete a request, then close it.
            {
                let mut conn1 = TcpStream::connect(addr).expect("connect 1");
                conn1.set_read_timeout(Some(Duration::from_secs(5))).ok();
                send_request(&mut conn1, "/hello");
                let s1 = read_status(&mut conn1);
                assert_eq!(s1, 200);
                // conn1 drops here — slot freed
            }

            // Wait for the server to notice the close and release the permit.
            std::thread::sleep(Duration::from_millis(200));

            // Second connection should succeed.
            let mut conn2 = TcpStream::connect(addr).expect("connect 2");
            conn2.set_read_timeout(Some(Duration::from_secs(5))).ok();
            send_request(&mut conn2, "/hello");
            let s2 = read_status(&mut conn2);
            assert_eq!(s2, 200);

            camber::runtime::request_shutdown();
        })
        .unwrap();
}
