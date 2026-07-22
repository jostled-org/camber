use crate::runtime_support as common;

use camber::http::{Request, Response, Router, compression};
use camber::runtime;
use flate2::read::GzDecoder;
use std::io::Read;
use std::time::Duration;

fn request(
    addr: std::net::SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
) -> crate::http::HttpResponse {
    crate::http::request(addr, "GET", path, headers, &[], Duration::from_secs(5)).unwrap()
}

fn large_text_body() -> String {
    "Hello, this is a response that is large enough to be compressed. ".repeat(50)
}

#[test]
fn compression_gzips_text_response() {
    common::test_runtime()
        .run(|| {
            let body = large_text_body();
            let expected = body.clone();

            let mut router = Router::new();
            router.use_middleware(compression::auto());
            router.get("/text", move |_req: &Request| {
                let body = body.clone();
                async move { Response::text(200, &body) }
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "/text", &[("Accept-Encoding", "gzip")]);

            assert_eq!(response.status, 200);
            assert_eq!(response.header("content-encoding"), Some("gzip"));

            let mut decoder = GzDecoder::new(response.body.as_ref());
            let mut decompressed = String::new();
            decoder.read_to_string(&mut decompressed).unwrap();
            assert_eq!(decompressed, expected);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn compression_skips_small_responses() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(compression::auto());
            router.get("/small", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "/small", &[("Accept-Encoding", "gzip")]);

            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"ok");
            assert!(
                response.header("content-encoding").is_none(),
                "small responses should not be compressed",
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn compression_skips_binary_responses() {
    common::test_runtime()
        .run(|| {
            let binary_data = vec![0u8; 2048];

            let mut router = Router::new();
            router.use_middleware(compression::auto());
            router.get("/binary", move |_req: &Request| {
                let binary_data = binary_data.clone();
                async move { Response::bytes(200, binary_data) }
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "/binary", &[("Accept-Encoding", "gzip")]);

            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), vec![0_u8; 2048].as_slice());
            assert!(
                response.header("content-encoding").is_none(),
                "binary responses should not be compressed",
            );

            runtime::request_shutdown();
        })
        .unwrap();
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
            let response = request(addr, "/text", &[]);

            assert_eq!(response.status, 200);
            assert!(
                response.header("content-encoding").is_none(),
                "should not compress without Accept-Encoding",
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
