mod common;

use camber::http::{Request, Response, Router, cors};
use camber::runtime;
use std::io::{Read, Write};

fn send_raw(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn find_header<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    let header_section = raw.split("\r\n\r\n").next()?;
    for line in header_section.split("\r\n") {
        if let Some((key, value)) = line.split_once(": ") {
            if key.eq_ignore_ascii_case(name) {
                return Some(value);
            }
        }
    }
    None
}

#[test]
fn cors_adds_origin_header_for_allowed_origin() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.get("/hello", |_req: &Request| async { Response::text(200, "ok") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /hello HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 200"));
        assert_eq!(
            find_header(&raw, "access-control-allow-origin"),
            Some("https://example.com"),
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_rejects_disallowed_origin() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.get("/hello", |_req: &Request| async { Response::text(200, "ok") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /hello HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.com\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 200"));
        assert!(
            find_header(&raw, "access-control-allow-origin").is_none(),
            "should not have ACAO header for disallowed origin",
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_handles_preflight_options() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.get("/api", |_req: &Request| async { Response::text(200, "data") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "OPTIONS /api HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        );

        assert!(
            raw.starts_with("HTTP/1.1 204"),
            "preflight should return 204, got: {raw}",
        );
        assert_eq!(
            find_header(&raw, "access-control-allow-origin"),
            Some("https://example.com"),
        );
        assert!(
            find_header(&raw, "access-control-allow-methods").is_some(),
            "preflight should include allow-methods header",
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_builder_customizes_methods_and_max_age() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(
            cors::builder()
                .origins(&["https://example.com"])
                .methods(&["GET", "POST"])
                .max_age(7200)
                .build(),
        );
        router.get("/api", |_req: &Request| async { Response::text(200, "data") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "OPTIONS /api HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 204"));
        assert_eq!(
            find_header(&raw, "access-control-allow-methods"),
            Some("GET, POST"),
        );
        assert_eq!(
            find_header(&raw, "access-control-max-age"),
            Some("7200"),
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_wildcard_takes_precedence_over_exact_origin_when_credentials_disabled() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(
            cors::builder()
                .origins(&["https://example.com", "*"])
                .build(),
        );
        router.get("/hello", |_req: &Request| async { Response::text(200, "ok") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /hello HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 200"));
        assert_eq!(
            find_header(&raw, "access-control-allow-origin"),
            Some("*"),
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_applies_to_proxy_response() {
    common::test_runtime().run(|| {
        let mut backend = Router::new();
        backend.get("/data", |_req: &Request| async { Response::text(200, "proxied-data") });
        let backend_addr = common::spawn_server(backend);

        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.proxy("/api", &format!("http://{backend_addr}"));

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            &format!(
                "GET /api/data HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nConnection: close\r\n\r\n"
            ),
        );

        assert!(
            raw.starts_with("HTTP/1.1 200"),
            "expected 200 proxied response, got: {raw}"
        );
        assert_eq!(
            find_header(&raw, "access-control-allow-origin"),
            Some("https://example.com"),
            "CORS header should be present on proxied response"
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_response_includes_vary_origin() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.get("/hello", |_req: &Request| async { Response::text(200, "ok") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /hello HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 200"));
        let vary = find_header(&raw, "vary").expect("Vary header must be present on CORS response");
        assert!(
            vary.contains("Origin"),
            "Vary must contain Origin, got: {vary}",
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_preflight_includes_vary_headers() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.get("/api", |_req: &Request| async { Response::text(200, "data") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "OPTIONS /api HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nAccess-Control-Request-Method: POST\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 204"));
        let vary = find_header(&raw, "vary").expect("Vary header must be present on preflight");
        assert!(
            vary.contains("Origin"),
            "Vary must contain Origin, got: {vary}",
        );
        assert!(
            vary.contains("Access-Control-Request-Method"),
            "Vary must contain Access-Control-Request-Method, got: {vary}",
        );
        assert!(
            vary.contains("Access-Control-Request-Headers"),
            "Vary must contain Access-Control-Request-Headers, got: {vary}",
        );

        runtime::request_shutdown();
    }).unwrap();
}

#[test]
fn cors_composes_with_other_middleware() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.use_middleware(cors::allow_origins(&["https://example.com"]));
        router.use_middleware(|req, next| {
            let fut = next.call(req);
            Box::pin(async move { fut.await.with_header("X-Custom", "present") })
        });
        router.get("/hello", |_req: &Request| async { Response::text(200, "ok") });

        let addr = common::spawn_server(router);
        let raw = send_raw(
            addr,
            "GET /hello HTTP/1.1\r\nHost: localhost\r\nOrigin: https://example.com\r\nConnection: close\r\n\r\n",
        );

        assert!(raw.starts_with("HTTP/1.1 200"));
        assert_eq!(
            find_header(&raw, "access-control-allow-origin"),
            Some("https://example.com"),
        );
        assert_eq!(
            find_header(&raw, "x-custom"),
            Some("present"),
        );

        runtime::request_shutdown();
    }).unwrap();
}
