mod common;

use camber::http::{CookieOptions, Request, Response, Router, SameSite};
use camber::runtime;
use std::io::{Read, Write};

// ── Step 1.T4: response_text_can_be_unwrapped_and_chained_with_set_cookie ──
#[test]
fn response_text_can_be_unwrapped_and_chained_with_set_cookie() {
    let resp = Response::text(200, "ok")
        .expect("valid status")
        .set_cookie("session_id", "abc123");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");

    let set_cookie = resp
        .headers()
        .iter()
        .find(|(k, _)| k.as_ref() == "Set-Cookie")
        .map(|(_, v)| v.as_ref());
    assert_eq!(set_cookie, Some("session_id=abc123"));
}

#[test]
fn request_cookie_parses_single_cookie() {
    let req = Request::builder()
        .header("Cookie", "sid=abc123")
        .finish()
        .expect("valid request");

    assert_eq!(req.cookie("sid"), Some("abc123"));
    assert_eq!(req.cookie("missing"), None);
}

#[test]
fn request_cookies_parses_multiple_cookies() {
    let req = Request::builder()
        .header("Cookie", "a=1; b=2; c=3")
        .finish()
        .expect("valid request");

    let cookies: Vec<(&str, &str)> = req.cookies().collect();
    assert_eq!(cookies.len(), 3);
    assert_eq!(req.cookie("a"), Some("1"));
    assert_eq!(req.cookie("b"), Some("2"));
    assert_eq!(req.cookie("c"), Some("3"));
}

#[test]
fn response_set_cookie_adds_header() {
    let resp = Response::text(200, "ok")
        .expect("valid status")
        .set_cookie("sid", "abc123");

    let set_cookie = resp
        .headers()
        .iter()
        .find(|(k, _)| k.as_ref() == "Set-Cookie")
        .map(|(_, v)| v.as_ref());
    assert_eq!(set_cookie, Some("sid=abc123"));
}

#[test]
fn response_set_cookie_with_options() {
    let options = CookieOptions::new()
        .path("/app")
        .max_age(3600)
        .secure()
        .http_only()
        .same_site(SameSite::Strict);

    let resp = Response::text(200, "ok")
        .expect("valid status")
        .set_cookie_with("sid", "abc", &options);

    let set_cookie = resp
        .headers()
        .iter()
        .find(|(k, _)| k.as_ref() == "Set-Cookie")
        .map(|(_, v)| v.as_ref())
        .unwrap();

    assert!(set_cookie.starts_with("sid=abc"));
    assert!(set_cookie.contains("Path=/app"));
    assert!(set_cookie.contains("Max-Age=3600"));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
}

#[test]
fn cookies_round_trip_through_server() {
    common::test_runtime().run(|| {
        let mut router = Router::new();
        router.get("/echo-cookie", |req: &Request| {
            let val = req.cookie("sid").map(|v| v.to_owned());
            async move {
                match val {
                    Some(v) => Response::text(200, &v),
                    None => Response::text(400, "no cookie"),
                }
            }
        });
        let addr = common::spawn_server(router);

        let mut stream = std::net::TcpStream::connect(addr).unwrap();
        let request = format!(
            "GET /echo-cookie HTTP/1.1\r\nHost: {addr}\r\nCookie: sid=session42\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();

        assert!(response.contains("200"));
        assert!(response.contains("session42"));

        runtime::request_shutdown();
    })
    .unwrap();
}
