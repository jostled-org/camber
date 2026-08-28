use crate::runtime_support as common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use camber::http::mock::{ConnectionOwnerEdge, connection_owner};

/// The bound every wait in this module fails on rather than blocking forever.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

fn send_request(stream: &mut TcpStream, path: &str, connection: Option<&str>) {
    let conn_header = match connection {
        Some(val) => format!("Connection: {val}\r\n"),
        None => String::new(),
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{conn_header}\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
}

fn assert_connection_eof(stream: &mut TcpStream, expected: &str) {
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Ok(count) => panic!("expected {expected}, but read {count} byte(s): {byte:?}"),
        Err(error) => panic!("expected {expected}, but the EOF read failed: {error}"),
    }
}

#[test]
fn keepalive_serves_multiple_requests_on_one_connection() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "Hello, world!")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            let mut stream = crate::http::connect(addr).expect("connect");

            // First request — no Connection header (HTTP/1.1 defaults to keep-alive)
            send_request(&mut stream, "/hello", None);
            let response =
                crate::http::read_http_response_bounded(&mut stream).expect("first response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello, world!");

            // Second request on the same connection
            send_request(&mut stream, "/hello", None);
            let response =
                crate::http::read_http_response_bounded(&mut stream).expect("second response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello, world!");

            // Third request with Connection: close
            send_request(&mut stream, "/hello", Some("close"));
            let response =
                crate::http::read_http_response_bounded(&mut stream).expect("third response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello, world!");

            // Server should have closed the connection
            assert_connection_eof(
                &mut stream,
                "server to close connection after Connection: close",
            );

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

/// An already-served keep-alive connection that goes quiet is closed on the
/// header boundary, not held open.
///
/// This is the dimension `header_timeout` inherited from `keepalive_timeout`,
/// and the only row that enters it through a connection the server has already
/// answered on. The pre-head row in `acceptance_e2e::service_deadlines` proves
/// the fresh-connection case: a peer that never finishes its first head. Neither
/// covers the other, and a server that started its header timer once per
/// connection instead of once per head would keep every idle keep-alive socket
/// until the process ran out of descriptors.
///
/// The rendezvous is production's own. `HeaderTimeoutConfigured` holds the
/// supervisor immediately before it hands this connection to Hyper, and it
/// carries the boundary it is about to configure — so a row naming a value the
/// server never resolved waits at a checkpoint production never reaches. The
/// close itself is then read off the socket: the bounded EOF read ends when the
/// server closes and fails when it does not, so nothing here sleeps to make the
/// expiry likely.
#[test]
fn header_timeout_closes_idle_keepalive_connection() {
    const IDLE_BOUND: Duration = Duration::from_millis(200);

    common::test_runtime()
        .header_timeout(IDLE_BOUND)
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "Hello")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();
            let connections =
                connection_owner(addr).expect("register the idle keep-alive listener");
            let configured = ConnectionOwnerEdge::HeaderTimeoutConfigured(IDLE_BOUND);
            connections
                .pause_once(configured)
                .expect("arm the configured header boundary");

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            // Connecting is all this thread does before the release: the
            // accepted socket reaches the checkpoint on its own, and nothing
            // is written until Hyper owns it.
            let mut stream = crate::http::connect(addr).expect("connect");
            common::block_on(async {
                tokio::time::timeout(EVENT_TIMEOUT, connections.wait_until_paused(configured))
                    .await
                    .expect("the served connection never reached its configured header boundary")
            })
            .expect("production header-boundary configuration returned Pending");
            connections
                .release(configured)
                .expect("release the configured header boundary");

            // One served request, no Connection header: the connection stays
            // open and the next head is what the boundary now bounds.
            send_request(&mut stream, "/hello", None);
            let response = crate::http::read_http_response_bounded(&mut stream).expect("response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello");

            // Nothing follows it. The server owes this idle connection a close.
            assert_connection_eof(
                &mut stream,
                "server to close an already-served idle connection on the header boundary",
            );

            drop(connections);
            camber::runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn connection_close_header_prevents_keepalive() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "Hello")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            let mut stream = crate::http::connect(addr).expect("connect");

            // Send request with Connection: close
            send_request(&mut stream, "/hello", Some("close"));
            let response = crate::http::read_http_response_bounded(&mut stream).expect("response");
            assert_eq!(response.status, 200);
            assert_eq!(response.header("connection"), Some("close"));

            // Server should have closed the connection
            assert_connection_eof(
                &mut stream,
                "server to close connection after Connection: close",
            );

            camber::runtime::request_shutdown();
        })
        .unwrap();
}
