mod common;

use camber::http::mock;
use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[camber::test]
async fn mock_http_intercepts_outbound_call() {
    let mock = mock::http("https://external-api/data")
        .returns(Response::json(200, &serde_json::json!({"key": "value"})).expect("valid status"));

    let mut router = Router::new();
    router.get("/proxy", |_req: &Request| async {
        let upstream = http::get("https://external-api/data").await?;
        Response::text(200, upstream.body())
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/proxy")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), r#"{"key":"value"}"#);

    mock.assert_called_once();

    runtime::request_shutdown();
}

#[test]
fn request_builder_constructs_test_request() {
    let req = Request::builder()
        .method("POST")
        .expect("valid method")
        .path("/users")
        .body("{}")
        .finish()
        .expect("valid request");

    assert_eq!(req.method(), "POST");
    assert_eq!(req.path(), "/users");
    assert_eq!(req.body(), "{}");
}
