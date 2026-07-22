use crate::runtime_support as common;

use camber::http::{Request, Response, Router, cors};
use camber::runtime;
use std::time::Duration;

fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> crate::http::HttpResponse {
    crate::http::request(addr, method, path, headers, &[], Duration::from_secs(5)).unwrap()
}

#[test]
fn cors_adds_origin_header_for_allowed_origin() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "GET", "/hello", &[("Origin", "https://example.com")]);

            assert_eq!(response.status, 200);
            assert_eq!(
                response.header("access-control-allow-origin"),
                Some("https://example.com"),
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_rejects_disallowed_origin() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "GET", "/hello", &[("Origin", "https://evil.com")]);

            assert_eq!(response.status, 200);
            assert!(
                response.header("access-control-allow-origin").is_none(),
                "should not have ACAO header for disallowed origin",
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_handles_preflight_options() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.get("/api", |_req: &Request| async {
                Response::text(200, "data")
            });

            let addr = common::spawn_server(router);
            let response = request(
                addr,
                "OPTIONS",
                "/api",
                &[
                    ("Origin", "https://example.com"),
                    ("Access-Control-Request-Method", "POST"),
                ],
            );

            assert_eq!(response.status, 204);
            assert_eq!(
                response.header("access-control-allow-origin"),
                Some("https://example.com"),
            );
            assert!(
                response.header("access-control-allow-methods").is_some(),
                "preflight should include allow-methods header",
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_builder_customizes_methods_and_max_age() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(
                cors::builder()
                    .origins(&["https://example.com"])
                    .methods(&["GET", "POST"])
                    .max_age(7200)
                    .build(),
            );
            router.get("/api", |_req: &Request| async {
                Response::text(200, "data")
            });

            let addr = common::spawn_server(router);
            let response = request(
                addr,
                "OPTIONS",
                "/api",
                &[
                    ("Origin", "https://example.com"),
                    ("Access-Control-Request-Method", "POST"),
                ],
            );

            assert_eq!(response.status, 204);
            assert_eq!(
                response.header("access-control-allow-methods"),
                Some("GET, POST"),
            );
            assert_eq!(response.header("access-control-max-age"), Some("7200"),);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_wildcard_takes_precedence_over_exact_origin_when_credentials_disabled() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(
                cors::builder()
                    .origins(&["https://example.com", "*"])
                    .build(),
            );
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "GET", "/hello", &[("Origin", "https://example.com")]);

            assert_eq!(response.status, 200);
            assert_eq!(response.header("access-control-allow-origin"), Some("*"),);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_applies_to_proxy_response() {
    common::test_runtime()
        .run(|| {
            let mut backend = Router::new();
            backend.get("/data", |_req: &Request| async {
                Response::text(200, "proxied-data")
            });
            let backend_addr = common::spawn_server(backend);

            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.proxy("/api", &format!("http://{backend_addr}"));

            let addr = common::spawn_server(router);
            let response = request(
                addr,
                "GET",
                "/api/data",
                &[("Origin", "https://example.com")],
            );

            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"proxied-data");
            assert_eq!(
                response.header("access-control-allow-origin"),
                Some("https://example.com"),
                "CORS header should be present on proxied response"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_response_includes_vary_origin() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "GET", "/hello", &[("Origin", "https://example.com")]);

            assert_eq!(response.status, 200);
            let vary = response
                .header("vary")
                .expect("Vary header must be present on CORS response");
            assert!(
                vary.contains("Origin"),
                "Vary must contain Origin, got: {vary}",
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cors_preflight_includes_vary_headers() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.get("/api", |_req: &Request| async {
                Response::text(200, "data")
            });

            let addr = common::spawn_server(router);
            let response = request(
                addr,
                "OPTIONS",
                "/api",
                &[
                    ("Origin", "https://example.com"),
                    ("Access-Control-Request-Method", "POST"),
                ],
            );

            assert_eq!(response.status, 204);
            let vary = response
                .header("vary")
                .expect("Vary header must be present on preflight");
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
        })
        .unwrap();
}

#[test]
fn cors_composes_with_other_middleware() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(cors::allow_origins(&["https://example.com"]));
            router.use_middleware(|req, next| {
                let fut = next.call(req);
                Box::pin(async move { fut.await.with_header("X-Custom", "present") })
            });
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let addr = common::spawn_server(router);
            let response = request(addr, "GET", "/hello", &[("Origin", "https://example.com")]);

            assert_eq!(response.status, 200);
            assert_eq!(
                response.header("access-control-allow-origin"),
                Some("https://example.com"),
            );
            assert_eq!(response.header("x-custom"), Some("present"),);

            runtime::request_shutdown();
        })
        .unwrap();
}
