use camber::http::{Request, Response, Router};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn delete_with_body_sends_body() {
    let mut router = Router::new();
    router.delete("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = camber::http::delete_with_body(&format!("http://{addr}/echo"), "delete-payload")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "delete-payload");
}

#[tokio::test(flavor = "multi_thread")]
async fn put_json_and_patch_json_send_json_content_type() {
    let mut router = Router::new();
    router.put("/echo", |req: &Request| {
        let ct = req.header("content-type").unwrap_or("").to_owned();
        let body = req.body().to_owned();
        async move { Response::text(200, &format!("{ct}|{body}")) }
    });
    router.patch("/echo", |req: &Request| {
        let ct = req.header("content-type").unwrap_or("").to_owned();
        let body = req.body().to_owned();
        async move { Response::text(200, &format!("{ct}|{body}")) }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    tokio::time::sleep(Duration::from_millis(50)).await;

    let base = format!("http://{addr}/echo");

    let put_resp = camber::http::put_json(&base, r#"{"key":"put"}"#)
        .await
        .unwrap();
    assert_eq!(put_resp.status(), 200);
    let put_body = put_resp.body();
    assert!(
        put_body.starts_with("application/json"),
        "put_json should send application/json, got: {put_body}"
    );
    assert!(put_body.contains(r#"{"key":"put"}"#));

    let patch_resp = camber::http::patch_json(&base, r#"{"key":"patch"}"#)
        .await
        .unwrap();
    assert_eq!(patch_resp.status(), 200);
    let patch_body = patch_resp.body();
    assert!(
        patch_body.starts_with("application/json"),
        "patch_json should send application/json, got: {patch_body}"
    );
    assert!(patch_body.contains(r#"{"key":"patch"}"#));
}

#[tokio::test(flavor = "multi_thread")]
async fn put_form_and_patch_form_send_urlencoded() {
    let mut router = Router::new();
    router.put("/echo", |req: &Request| {
        let ct = req.header("content-type").unwrap_or("").to_owned();
        let body = req.body().to_owned();
        async move { Response::text(200, &format!("{ct}|{body}")) }
    });
    router.patch("/echo", |req: &Request| {
        let ct = req.header("content-type").unwrap_or("").to_owned();
        let body = req.body().to_owned();
        async move { Response::text(200, &format!("{ct}|{body}")) }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    tokio::time::sleep(Duration::from_millis(50)).await;

    let base = format!("http://{addr}/echo");

    let put_resp = camber::http::put_form(&base, "a=1&b=2").await.unwrap();
    assert_eq!(put_resp.status(), 200);
    let put_body = put_resp.body();
    assert!(
        put_body.starts_with("application/x-www-form-urlencoded"),
        "put_form should send urlencoded, got: {put_body}"
    );
    assert!(put_body.contains("a=1&b=2"));

    let patch_resp = camber::http::patch_form(&base, "c=3&d=4").await.unwrap();
    assert_eq!(patch_resp.status(), 200);
    let patch_body = patch_resp.body();
    assert!(
        patch_body.starts_with("application/x-www-form-urlencoded"),
        "patch_form should send urlencoded, got: {patch_body}"
    );
    assert!(patch_body.contains("c=3&d=4"));
}
