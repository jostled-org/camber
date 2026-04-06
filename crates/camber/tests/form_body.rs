mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[test]
fn form_extracts_field_from_post_body() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/login", |req: &Request| {
                let username = req.form("username").unwrap_or("none").to_owned();
                async move { Response::text(200, &username) }
            });

            let addr = common::spawn_server(router);
            let resp = common::block_on(http::post_form(
                &format!("http://{addr}/login"),
                "username=alice&password=secret",
            ))
            .unwrap();

            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "alice");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn form_returns_none_when_missing() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/login", |req: &Request| {
                let missing = req.form("missing").unwrap_or("none").to_owned();
                async move { Response::text(200, &missing) }
            });

            let addr = common::spawn_server(router);
            let resp = common::block_on(http::post_form(
                &format!("http://{addr}/login"),
                "username=alice",
            ))
            .unwrap();

            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "none");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn form_decodes_percent_encoding() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.post("/submit", |req: &Request| {
                let message = req.form("message").unwrap_or("none").to_owned();
                async move { Response::text(200, &message) }
            });

            let addr = common::spawn_server(router);
            let resp = common::block_on(http::post_form(
                &format!("http://{addr}/submit"),
                "message=hello%20world%21",
            ))
            .unwrap();

            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "hello world!");

            runtime::request_shutdown();
        })
        .unwrap();
}
