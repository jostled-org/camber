mod common;

use camber::http::{self, Request, Response, Router, mock};
use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::time::Duration;

// ── Step 1.T3: client_builder_sends_requests_without_build_step ──
#[camber::test]
async fn client_builder_sends_requests_without_build_step() {
    let mut backend = Router::new();
    backend.get("/data", |_req: &Request| async {
        Response::text(200, "from upstream")
    });
    let upstream_addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(1)
        .get(&format!("http://{upstream_addr}/data"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "from upstream");

    runtime::request_shutdown();
}

// --- Step 2 TDD tests: HTTP client is async-only ---

#[camber::test]
async fn http_get_is_async() {
    let mut backend = Router::new();
    backend.get("/upstream", |_req: &Request| async {
        Response::text(200, "hello from upstream")
    });
    let upstream_addr = common::spawn_server(backend);

    let mut proxy = Router::new();
    let url: Arc<str> = format!("http://{upstream_addr}/upstream").into();
    proxy.get("/proxy", move |_req: &Request| {
        let url = Arc::clone(&url);
        async move {
            let resp = http::get(&url).await?;
            Response::text(resp.status(), resp.body()) as Result<Response, RuntimeError>
        }
    });
    let proxy_addr = common::spawn_server(proxy);

    let resp = http::get(&format!("http://{proxy_addr}/proxy"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello from upstream");

    runtime::request_shutdown();
}

#[camber::test]
async fn http_post_is_async() {
    let mut backend = Router::new();
    backend.post("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let upstream_addr = common::spawn_server(backend);

    let mut proxy = Router::new();
    let url: Arc<str> = format!("http://{upstream_addr}/echo").into();
    proxy.post("/proxy", move |_req: &Request| {
        let url = Arc::clone(&url);
        async move {
            let resp = http::post(&url, "async body").await?;
            Response::text(resp.status(), resp.body()) as Result<Response, RuntimeError>
        }
    });
    let proxy_addr = common::spawn_server(proxy);

    let resp = http::post(&format!("http://{proxy_addr}/proxy"), "")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "async body");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_builder_retries_async() {
    let call_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&call_count);

    let mut backend = Router::new();
    backend.get("/flaky", move |_req: &Request| {
        let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            match n < 2 {
                true => Response::empty(503),
                false => Response::text(200, "recovered"),
            }
        }
    });
    let upstream_addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{upstream_addr}/flaky"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "recovered");
    assert!(call_count.load(std::sync::atomic::Ordering::SeqCst) >= 3);

    runtime::request_shutdown();
}

// --- Existing client tests migrated to async ---

#[camber::test]
async fn client_get_returns_response() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });
    let addr = common::spawn_server(backend);

    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_post_returns_response() {
    let mut backend = Router::new();
    backend.post("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::post(&format!("http://{addr}/echo"), "data")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "data");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_post_json_sets_content_type() {
    let mut backend = Router::new();
    backend.post("/check-ct", |req: &Request| {
        let ct = req
            .headers()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        async move { Response::text(200, &ct) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::post_json(&format!("http://{addr}/check-ct"), "{}")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "application/json");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_builder_timeout() {
    let mut backend = Router::new();
    backend.get("/slow", |_req: &Request| async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Response::text(200, "slow")
    });
    let addr = common::spawn_server(backend);

    let result = http::client()
        .read_timeout(Duration::from_millis(100))
        .get(&format!("http://{addr}/slow"))
        .await;

    match result {
        Err(RuntimeError::Timeout) => {}
        Err(e) => panic!("expected Timeout error, got: {e}"),
        Ok(_) => panic!("expected Timeout error, got Ok"),
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn client_builder_reuses_built_client() {
    let mut backend = Router::new();
    backend.get("/ping", |_req: &Request| async {
        Response::text(200, "pong")
    });
    let addr = common::spawn_server(backend);

    let builder = http::client().read_timeout(Duration::from_secs(5));

    let resp1 = builder.get(&format!("http://{addr}/ping")).await.unwrap();
    assert_eq!(resp1.status(), 200);
    assert_eq!(resp1.body(), "pong");

    let resp2 = builder.get(&format!("http://{addr}/ping")).await.unwrap();
    assert_eq!(resp2.status(), 200);
    assert_eq!(resp2.body(), "pong");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_put_sends_body_and_returns_response() {
    let mut backend = Router::new();
    backend.put("/resource", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::put(&format!("http://{addr}/resource"), "payload")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "payload");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_delete_sends_request() {
    let mut backend = Router::new();
    backend.delete("/resource", |_req: &Request| async { Response::empty(204) });
    let addr = common::spawn_server(backend);

    let resp = http::delete(&format!("http://{addr}/resource"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_patch_sends_body() {
    let mut backend = Router::new();
    backend.patch("/resource", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::patch(&format!("http://{addr}/resource"), "partial update")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "partial update");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_head_returns_headers_without_body() {
    let mut backend = Router::new();
    backend.head("/resource", |_req: &Request| async {
        Response::empty(200).map(|r| r.with_header("x-custom", "present"))
    });
    let addr = common::spawn_server(backend);

    let resp = http::head(&format!("http://{addr}/resource"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.body().is_empty());
    assert!(
        resp.headers()
            .iter()
            .any(|(k, v)| k.as_ref() == "x-custom" && v.as_ref() == "present")
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn client_options_returns_allowed_methods() {
    let mut backend = Router::new();
    backend.options("/resource", |_req: &Request| async {
        Response::empty(200).map(|r| r.with_header("allow", "GET, POST, OPTIONS"))
    });
    let addr = common::spawn_server(backend);

    let resp = http::options(&format!("http://{addr}/resource"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers()
            .iter()
            .any(|(k, v)| k.as_ref() == "allow" && v.as_ref() == "GET, POST, OPTIONS")
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn client_builder_put_with_custom_timeout() {
    let mut backend = Router::new();
    backend.put("/resource", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let addr = common::spawn_server(backend);

    let builder = http::client().read_timeout(Duration::from_secs(5));
    let resp = builder
        .put(&format!("http://{addr}/resource"), "builder body")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "builder body");

    runtime::request_shutdown();
}

#[camber::test]
async fn client_mock_still_works() {
    let mock = mock::http("http://mock.test/api")
        .returns(Response::text(200, "mocked").expect("valid status"));

    let resp = http::get("http://mock.test/api").await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "mocked");
    mock.assert_called_once();

    runtime::request_shutdown();
}
