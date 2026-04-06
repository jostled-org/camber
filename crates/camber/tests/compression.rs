mod common;

use camber::http::{Request, Response, Router, compression};
use camber::runtime;
use flate2::read::GzDecoder;
use std::io::{Read, Write};

fn send_raw(addr: std::net::SocketAddr, request: &str) -> Vec<u8> {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    response
}

fn split_headers_body(raw: &[u8]) -> (&[u8], &[u8]) {
    let delimiter = b"\r\n\r\n";
    for i in 0..raw.len().saturating_sub(delimiter.len()) {
        if &raw[i..i + delimiter.len()] == delimiter {
            return (&raw[..i], &raw[i + delimiter.len()..]);
        }
    }
    (raw, &[])
}

fn find_header<'a>(header_section: &'a str, name: &str) -> Option<&'a str> {
    for line in header_section.split("\r\n") {
        if let Some((key, value)) = line.split_once(": ") {
            if key.eq_ignore_ascii_case(name) {
                return Some(value);
            }
        }
    }
    None
}

fn large_text_body() -> String {
    "Hello, this is a response that is large enough to be compressed. ".repeat(50)
}

#[test]
fn compression_gzips_text_response() {
    common::test_runtime().run(|| {
        let body = large_text_body();
        let expected = body.clone();

        let mut router = Router::new();
        router.use_middleware(compression::auto());
        router.get("/text", move |_req: &Request| {
            let body = body.clone();
            async move { Response::text(200, &body) }
        });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /text HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
        );

        let (headers_bytes, body_bytes) = split_headers_body(&raw);
        let headers_str = std::str::from_utf8(headers_bytes).unwrap();

        assert!(headers_str.starts_with("HTTP/1.1 200"));
        assert_eq!(find_header(headers_str, "content-encoding"), Some("gzip"));

        let mut decoder = GzDecoder::new(body_bytes);
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert_eq!(decompressed, expected);

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn compression_skips_small_responses() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(compression::auto());
        router.get("/small", |_req: &Request| async { Response::text(200, "ok") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /small HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
        );

        let (headers_bytes, _) = split_headers_body(&raw);
        let headers_str = std::str::from_utf8(headers_bytes).unwrap();

        assert!(headers_str.starts_with("HTTP/1.1 200"));
        assert!(
            find_header(headers_str, "content-encoding").is_none(),
            "small responses should not be compressed",
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn compression_skips_binary_responses() {
    common::test_runtime().run(|| {
        let binary_data = vec![0u8; 2048];

        let mut router = Router::new();
        router.use_middleware(compression::auto());
        router.get("/binary", move |_req: &Request| {
            let binary_data = binary_data.clone();
            async move { Response::bytes(200, binary_data) }
        });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /binary HTTP/1.1\r\nHost: localhost\r\nAccept-Encoding: gzip\r\nConnection: close\r\n\r\n",
        );

        let (headers_bytes, _) = split_headers_body(&raw);
        let headers_str = std::str::from_utf8(headers_bytes).unwrap();

        assert!(headers_str.starts_with("HTTP/1.1 200"));
        assert!(
            find_header(headers_str, "content-encoding").is_none(),
            "binary responses should not be compressed",
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn compression_respects_missing_accept_encoding() {
    common::test_runtime()
        .run(|| {
            let body = large_text_body();

            let mut router = Router::new();
            router.use_middleware(compression::auto());
            router.get("/text", move |_req: &Request| {
                let body = body.clone();
                async move { Response::text(200, &body) }
            });

            let addr = common::spawn_server(router);
            let raw = send_raw(
                addr,
                "GET /text HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            );

            let (headers_bytes, _) = split_headers_body(&raw);
            let headers_str = std::str::from_utf8(headers_bytes).unwrap();

            assert!(headers_str.starts_with("HTTP/1.1 200"));
            assert!(
                find_header(headers_str, "content-encoding").is_none(),
                "should not compress without Accept-Encoding",
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
