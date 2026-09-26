use std::io::{self, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use crate::source_failure::{SOURCE_FAILURE_BOUND, exchange_after_prefix};

fn accept(listener: &TcpListener) -> TcpStream {
    let mut peer = None;
    assert!(crate::http::poll_until(SOURCE_FAILURE_BOUND, || {
        match listener.accept() {
            Ok((stream, _)) => {
                peer = Some(stream);
                true
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }));
    peer.expect("accepted one peer")
}

fn with_response(answer: &[u8], check: impl FnOnce(SocketAddr)) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::scope(|scope| {
        let server = scope.spawn(move || {
            let mut peer = accept(&listener);
            peer.set_nonblocking(false).unwrap();
            peer.set_write_timeout(Some(SOURCE_FAILURE_BOUND)).unwrap();
            crate::http::read_head(&mut peer, SOURCE_FAILURE_BOUND).unwrap();
            peer.write_all(answer).unwrap();
        });
        check(addr);
        server.join().unwrap();
    });
}

#[test]
fn missing_prefix_does_not_release_the_source() {
    with_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab", |addr| {
        let mut released = false;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            exchange_after_prefix(addr, "/", "missing prefix", 3, || released = true)
        }));
        assert!(
            !released,
            "the source was released before its prefix arrived"
        );
        let error = outcome.expect_err("a partial prefix must fail the fixture");
        assert!(
            crate::http::panic_text(error.as_ref())
                .contains("the prefix did not arrive before release")
        );
    });
}

#[test]
fn prefix_and_remainder_are_retained_once() {
    with_response(
        b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nabcdef",
        |addr| {
            let mut releases = 0;
            let exchange = exchange_after_prefix(addr, "/", "complete prefix", 3, || releases += 1);
            assert_eq!(releases, 1);
            assert_eq!(exchange.status, 200);
            assert_eq!(exchange.declared, Some(9));
            assert_eq!(exchange.body.as_ref(), b"abcdef");
            crate::source_failure::assert_incomplete_framing(&exchange, "complete prefix");
        },
    );
}

#[test]
fn empty_prefix_releases_after_the_head() {
    with_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\n", |addr| {
        let mut releases = 0;
        let exchange = exchange_after_prefix(addr, "/", "empty prefix", 0, || releases += 1);
        assert_eq!(releases, 1);
        assert_eq!(exchange.status, 200);
        assert!(exchange.body.is_empty());
    });
}
