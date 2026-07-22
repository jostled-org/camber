use camber::http::{CookieOptions, HostRouter, Method, Request, Response, Router};

#[test]
fn request_debug_shows_method_and_path() {
    let req = Request::builder()
        .method("POST")
        .unwrap()
        .path("/api/users")
        .header("Content-Type", "application/json")
        .body(r#"{"name":"test"}"#)
        .finish()
        .unwrap();
    let debug = format!("{req:?}");
    assert!(debug.contains("POST"), "should contain method: {debug}");
    assert!(debug.contains("/api/users"), "should contain path: {debug}");
    assert!(
        !debug.contains("hyper"),
        "should not expose hyper internals: {debug}"
    );
    assert!(
        !debug.contains("HeaderMap"),
        "should not expose HeaderMap: {debug}"
    );
}

#[test]
fn response_debug_shows_status_and_body_length() {
    let resp = Response::text(200, "hello").unwrap();
    let debug = format!("{resp:?}");
    assert!(debug.contains("200"), "should contain status: {debug}");
}

#[test]
fn method_display_and_from_str_round_trip() {
    let variants = [
        (Method::Get, "GET"),
        (Method::Post, "POST"),
        (Method::Put, "PUT"),
        (Method::Delete, "DELETE"),
        (Method::Patch, "PATCH"),
        (Method::Head, "HEAD"),
        (Method::Options, "OPTIONS"),
    ];
    for (variant, expected) in variants {
        let displayed = format!("{variant}");
        assert_eq!(displayed, expected);

        let parsed: Method = expected.parse().unwrap();
        assert_eq!(parsed, variant);
    }

    let result: Result<Method, _> = "BOGUS".parse();
    assert!(result.is_err());
}

#[test]
fn cookie_options_clone() {
    let original = CookieOptions::new()
        .path("/app")
        .domain("example.com")
        .max_age(3600)
        .secure()
        .http_only();

    let cloned = original.clone();

    // Both original and clone should work independently with set_cookie_with.
    let resp_original = Response::empty(200)
        .unwrap()
        .set_cookie_with("sess", "abc", &original);
    let resp_cloned = Response::empty(200)
        .unwrap()
        .set_cookie_with("sess", "abc", &cloned);

    // Both should produce identical headers.
    let original_cookie = resp_original
        .headers()
        .iter()
        .find(|(k, _)| k == "Set-Cookie")
        .map(|(_, v)| v.as_ref());
    let cloned_cookie = resp_cloned
        .headers()
        .iter()
        .find(|(k, _)| k == "Set-Cookie")
        .map(|(_, v)| v.as_ref());
    assert_eq!(original_cookie, cloned_cookie);
    assert!(
        original_cookie.unwrap().contains("Path=/app"),
        "should contain original path"
    );
}

#[cfg(feature = "ws")]
#[test]
fn ws_message_clone() {
    use camber::http::WsMessage;

    let text = WsMessage::Text("hello".into());
    let text_clone = text.clone();
    match (&text, &text_clone) {
        (WsMessage::Text(a), WsMessage::Text(b)) => assert_eq!(a, b),
        _ => panic!("clone changed variant"),
    }

    let binary = WsMessage::Binary(bytes::Bytes::from_static(b"data"));
    let binary_clone = binary.clone();
    match (&binary, &binary_clone) {
        (WsMessage::Binary(a), WsMessage::Binary(b)) => assert_eq!(a, b),
        _ => panic!("clone changed variant"),
    }
}

#[test]
fn all_public_types_implement_debug() {
    // Request
    let req = Request::builder().finish().unwrap();
    let _ = format!("{req:?}");

    // Response
    let resp = Response::text(200, "ok").unwrap();
    let _ = format!("{resp:?}");

    // Method
    let _ = format!("{:?}", Method::Get);

    // Router
    let router = Router::new();
    let _ = format!("{router:?}");

    // HostRouter
    let host_router = HostRouter::new();
    let _ = format!("{host_router:?}");

    // ClientBuilder
    let cb = camber::http::client();
    let _ = format!("{cb:?}");

    // CookieOptions
    let co = CookieOptions::new();
    let _ = format!("{co:?}");

    // CorsBuilder
    let cors = camber::http::cors::builder();
    let _ = format!("{cors:?}");

    // RuntimeBuilder
    let rb = camber::runtime::builder();
    let _ = format!("{rb:?}");

    // RequestBuilder (via Debug format before finish)
    let rb = Request::builder().path("/test");
    let _ = format!("{rb:?}");
}
