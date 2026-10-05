use crate::runtime_support as common;

use camber::http::mock;
use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[tokio::test(start_paused = true)]
async fn address_reuse_bound_uses_real_time_when_runtime_time_is_paused() {
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = held.local_addr().unwrap();
    let bound = std::time::Duration::from_millis(50);
    let started = std::time::Instant::now();
    let result = crate::http::rebind_within(addr, bound).await;
    assert!(result.is_err(), "a held listener must prevent reuse");
    assert!(
        started.elapsed() >= bound,
        "virtual time exhausted a real socket bound"
    );
    drop(held);
    let rebound = crate::http::rebind_within(addr, bound).await.unwrap();
    drop(rebound);
}

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

/// Two live mocks can name the same URL and method. Dropping one deregisters
/// that one and leaves the other intercepting.
///
/// The registry is process-global, so deregistering by (url, method) took both
/// out: the survivor's handle then counted nothing while the real network call
/// went out.
#[camber::test]
async fn dropping_one_mock_leaves_its_twin_registered() {
    let url = "https://twin-api/data";
    let first = mock::http(url).returns(Response::text(200, "first").expect("valid status"));
    let second = mock::http(url).returns(Response::text(200, "second").expect("valid status"));

    let resp = http::get(url).await.expect("first mock intercepts");
    assert_eq!(resp.body(), "first");
    first.assert_called_once();

    drop(first);

    let resp = http::get(url).await.expect("second mock still intercepts");
    assert_eq!(resp.body(), "second");
    second.assert_called_once();

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
