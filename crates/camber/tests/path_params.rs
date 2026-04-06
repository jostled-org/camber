mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[camber::test]
async fn route_extracts_single_path_param() {
    let mut router = Router::new();
    router.get("/users/:id", |req: &Request| {
        let id = req.param("id").unwrap_or("missing").to_owned();
        async move { Response::text(200, &id) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/users/42")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "42");

    runtime::request_shutdown();
}

#[camber::test]
async fn route_extracts_multiple_path_params() {
    let mut router = Router::new();
    router.get("/users/:user_id/posts/:post_id", |req: &Request| {
        let user_id = req.param("user_id").unwrap_or("?").to_owned();
        let post_id = req.param("post_id").unwrap_or("?").to_owned();
        async move { Response::text(200, &format!("{user_id}:{post_id}")) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/users/7/posts/99"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "7:99");

    runtime::request_shutdown();
}

#[camber::test]
async fn dispatch_wildcard_route_captures_remainder() {
    let mut router = Router::new();
    router.get("/files/*path", |req: &Request| {
        let path = req.param("path").unwrap_or("missing").to_owned();
        async move { Response::text(200, &path) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/files/a/b/c"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "a/b/c");

    runtime::request_shutdown();
}

#[camber::test]
async fn static_routes_still_match_exactly() {
    let mut router = Router::new();
    router.get("/users/me", |_req: &Request| async {
        Response::text(200, "me-handler")
    });
    router.get("/users/:id", |req: &Request| {
        let id = req.param("id").unwrap_or("missing").to_owned();
        async move { Response::text(200, &id) }
    });

    let addr = common::spawn_server(router);

    let resp_static = http::get(&format!("http://{addr}/users/me")).await.unwrap();
    assert_eq!(resp_static.status(), 200);
    assert_eq!(resp_static.body(), "me-handler");

    let resp_param = http::get(&format!("http://{addr}/users/42")).await.unwrap();
    assert_eq!(resp_param.status(), 200);
    assert_eq!(resp_param.body(), "42");

    runtime::request_shutdown();
}
