use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The bound every fixture server in this module starts, shuts down, and joins
/// within.
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(2);

/// What a handler reports for a request target that carries no `?` at all.
///
/// A sentinel rather than an empty body, because an explicit empty query is a
/// distinct accepted representation that also produces an empty body.
const ABSENT: &str = "<absent>";

/// The query the head-only and streaming-proxy journeys send.
///
/// Every spelling here survives a URL parser unchanged, so the assertion
/// measures Camber's forwarding rather than a client's normalization.
const OBSERVED_QUERY: &str = "tag=a&tag=b&=blank&sp=a+b";

/// What [`query_identity`] must report for [`OBSERVED_QUERY`].
const OBSERVED_IDENTITY: &str = "tag=a&tag=b&=blank&sp=a+b tag=a,tag=b,=blank,sp=a b a";

/// Every raw and decoded query view a handler can read, as one line.
///
/// Stated once for the head-only SSE handler and the streaming-proxy gate: both
/// prove that a `RequestHead`-built request carries the same query contract, so
/// both have to observe it the same way.
fn query_identity(req: &Request) -> Box<str> {
    let raw = req.raw_query().unwrap_or(ABSENT);
    let pairs = req
        .query_pairs()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    let keyed = req.query("tag").unwrap_or(ABSENT);
    format!("{raw} {pairs} {keyed}").into_boxed_str()
}

/// A router whose one route answers with the raw query it received.
///
/// The buffered raw-target case and the streaming proxy's backend both measure
/// the exact spelling that reached a handler, so both read it the same way and
/// differ only in the path they serve.
fn raw_query_echo_router(path: &str) -> Router {
    let mut router = Router::new();
    router.get(path, |req: &Request| {
        let raw: Box<str> = req.raw_query().unwrap_or(ABSENT).into();
        async move { Response::text(200, &raw) }
    });
    router
}

/// Send one literal request target over its own socket and read the response.
///
/// The exact characters after `?` are the subject, so the request is written to
/// the wire rather than built by a client that could normalize them. The socket
/// is owned by this call and closed when it returns; `write_request` asks for
/// `Connection: close`, so each case gets a fresh connection.
fn literal_target_response(addr: SocketAddr, target: &str) -> crate::http::HttpResponse {
    tokio::task::block_in_place(|| {
        let mut stream = crate::http::connect(addr).expect("connect to the query fixture");
        crate::http::write_request(&mut stream, "GET", target, &[], &[])
            .expect("write the literal request target");
        crate::http::read_http_response_bounded(&mut stream).expect("read the bounded response")
    })
}

/// Shut one fixture server down within the bound and prove it cleaned up.
fn shutdown_and_assert_clean(server: crate::http::ReadyServer, name: &str) {
    let probe = server.cleanup_probe();
    server
        .shutdown_bounded(FIXTURE_TIMEOUT)
        .unwrap_or_else(|error| panic!("{name} server shut down within its bound: {error}"));
    assert!(probe.joined(), "{name} server joined");
    assert_eq!(probe.cleanup_error(), None, "{name} server cleanup error");
}

// ── Step 1.T2: query_all_returns_iterator_over_repeated_values ──
#[camber::test]
async fn query_all_returns_iterator_over_repeated_values() {
    let mut router = Router::new();
    router.get("/tags", |req: &Request| {
        let joined: String = req.query_all("tag").collect::<Vec<_>>().join(",");
        async move { Response::text(200, &joined) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/tags?tag=a&tag=b&tag=c"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "a,b,c");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_extracts_single_value() {
    let mut router = Router::new();
    router.get("/search", |req: &Request| {
        let q = req.query("q").unwrap_or("none").to_owned();
        async move { Response::text(200, &q) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/search?q=hello"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_returns_none_when_missing() {
    let mut router = Router::new();
    router.get("/search", |req: &Request| {
        let q = req.query("missing").unwrap_or("none").to_owned();
        async move { Response::text(200, &q) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/search?q=hello"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "none");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_handles_multiple_values() {
    let mut router = Router::new();
    router.get("/filter", |req: &Request| {
        let tags = req.query_all("tag").collect::<Vec<_>>().join(",");
        async move { Response::text(200, &tags) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/filter?tag=rust&tag=go"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "rust,go");

    runtime::request_shutdown();
}

/// Query spellings a client could normalize but the wire must carry verbatim.
const RAW_QUERY_CASES: &[&str] = &[
    "",
    "x=%2f+%20",
    "X=%2F&x=%2f",
    "a.b=1&a.b=2&=blank&bare&name=&=",
    "q=%E2%9C%93&bad=%zz&raw=%FF",
    "&&x=1&&",
];

#[tokio::test(flavor = "multi_thread")]
async fn raw_query_round_trips_literal_http_target() {
    let server =
        crate::http::spawn_server_ready(raw_query_echo_router("/items"), FIXTURE_TIMEOUT).unwrap();
    let addr = server.local_addr();

    let absent = literal_target_response(addr, "/items");
    assert_eq!(absent.status, 200);
    assert_eq!(
        absent.text().as_ref(),
        ABSENT,
        "a target with no ? has no raw query"
    );

    for case in RAW_QUERY_CASES {
        let target = format!("/items?{case}");
        let response = literal_target_response(addr, &target);
        assert_eq!(response.status, 200, "status for {target}");
        assert_eq!(
            response.text().as_ref(),
            *case,
            "raw query for the literal target {target}"
        );
    }

    shutdown_and_assert_clean(server, "raw-target");
}

#[tokio::test(flavor = "multi_thread")]
async fn head_only_handler_observes_query_identity() {
    let mut router = Router::new();
    router.get_sse(
        "/observe",
        |req: &Request, writer: &mut camber::http::SseWriter| {
            writer.event("query", &query_identity(req))?;
            Ok(())
        },
    );

    let server = crate::http::spawn_server_ready(router, FIXTURE_TIMEOUT).unwrap();
    let response =
        literal_target_response(server.local_addr(), &format!("/observe?{OBSERVED_QUERY}"));

    assert_eq!(response.status, 200);
    assert_eq!(
        response.text().as_ref(),
        format!("event: query\ndata: {OBSERVED_IDENTITY}\n\n"),
        "the head-only SSE request carries the same raw and decoded query contract"
    );

    shutdown_and_assert_clean(server, "head-only");
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_proxy_gate_observes_and_forwards_query_identity() {
    let backend_server =
        crate::http::spawn_server_ready(raw_query_echo_router("/echo"), FIXTURE_TIMEOUT).unwrap();

    // False until the gate validates the request. Loaded after the response, so
    // a forwarding path that skipped the middleware fails the test even though
    // the backend still answered.
    let gate_reached = Arc::new(AtomicBool::new(false));
    let witness = Arc::clone(&gate_reached);

    let mut proxy = Router::new();
    proxy.use_middleware(move |req, next| {
        match query_identity(req).as_ref() == OBSERVED_IDENTITY {
            true => {
                witness.store(true, Ordering::Release);
                next.call(req)
            }
            false => Box::pin(async {
                Response::text(460, "the gate observed a different query").expect("valid status")
            }),
        }
    });
    proxy.proxy_stream("/api", &format!("http://{}", backend_server.local_addr()));
    let proxy_server = crate::http::spawn_server_ready(proxy, FIXTURE_TIMEOUT).unwrap();

    let response = literal_target_response(
        proxy_server.local_addr(),
        &format!("/api/echo?{OBSERVED_QUERY}"),
    );

    assert_eq!(response.status, 200, "the gate admitted the request");
    assert_eq!(
        response.text().as_ref(),
        OBSERVED_QUERY,
        "the streaming proxy forwards the query spelling unchanged"
    );
    assert!(
        gate_reached.load(Ordering::Acquire),
        "the streaming-proxy middleware gate observed the request"
    );

    shutdown_and_assert_clean(proxy_server, "streaming-proxy");
    shutdown_and_assert_clean(backend_server, "proxy-backend");
}

#[camber::test]
async fn query_param_decodes_percent_encoding() {
    let mut router = Router::new();
    router.get("/search", |req: &Request| {
        let q = req.query("q").unwrap_or("none").to_owned();
        async move { Response::text(200, &q) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/search?q=hello%20world"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello world");

    runtime::request_shutdown();
}
