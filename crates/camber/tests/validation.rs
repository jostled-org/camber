mod common;

use camber::http::validate;
use camber::http::{Request, Response, Router};
use camber::runtime;
use serde::Deserialize;

#[derive(Deserialize)]
struct CreateUser {
    name: Box<str>,
    email: Box<str>,
}

#[camber::test]
async fn validation_passes_valid_json() {
    let mut router = Router::new();
    router.use_middleware(validate::json::<CreateUser>());
    router.post("/users", |req: &Request| {
        let user: CreateUser = req.json().unwrap();
        async move {
            assert!(!user.email.is_empty());
            Response::text(200, &user.name)
        }
    });
    let addr = common::spawn_server(router);

    let body = r#"{"name":"alice","email":"alice@example.com"}"#;
    let resp = camber::http::post_json(&format!("http://{addr}/users"), body)
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "alice");

    runtime::request_shutdown();
}

#[camber::test]
async fn validation_rejects_invalid_json() {
    let mut router = Router::new();
    router.use_middleware(validate::json::<CreateUser>());
    router.post("/users", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let addr = common::spawn_server(router);

    let resp = camber::http::post(&format!("http://{addr}/users"), "not valid json")
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    assert!(
        resp.body().contains("expected"),
        "error body: {}",
        resp.body()
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn validation_skips_non_post_requests() {
    let mut router = Router::new();
    router.use_middleware(validate::json::<CreateUser>());
    router.get("/users", |_req: &Request| async {
        Response::text(200, "list")
    });
    let addr = common::spawn_server(router);

    let resp = camber::http::get(&format!("http://{addr}/users"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "list");

    runtime::request_shutdown();
}

#[camber::test]
async fn validation_caches_parse_result() {
    let mut router = Router::new();
    router.use_middleware(validate::json::<CreateUser>());
    router.post("/users", |req: &Request| {
        let first: CreateUser = req.json().unwrap();
        let second: CreateUser = req.json().unwrap();
        async move {
            assert_eq!(&*first.name, &*second.name);
            Response::text(200, &first.name)
        }
    });
    let addr = common::spawn_server(router);

    let body = r#"{"name":"bob","email":"bob@example.com"}"#;
    let resp = camber::http::post_json(&format!("http://{addr}/users"), body)
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "bob");

    runtime::request_shutdown();
}
