use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::num::NonZeroUsize;
use std::time::Duration;

const GENERATED_ROUTE_CASES: u64 = 48;
const ROUTE_MATRIX_BOUND: NonZeroUsize = NonZeroUsize::new(5).unwrap();
const CAPTURE_BOUND: NonZeroUsize = NonZeroUsize::new(4).unwrap();
const CAPTURES: [&str; 4] = ["alpha", "bravo", "charlie", "delta"];

fn route_matrix_router() -> Router {
    let mut router = Router::new();

    // Register least-specific first. Lookup precedence, not insertion order,
    // must select static, then parameter, then wildcard routes.
    router.get("/matrix/*tail", |req: &Request| {
        let tail = req.param("tail").unwrap_or("missing").to_owned();
        async move { Response::text(200, &format!("wildcard:{tail}")) }
    });
    router.get("/matrix/:item/:leaf", |req: &Request| {
        let item = req.param("item").unwrap_or("missing").to_owned();
        let leaf = req.param("leaf").unwrap_or("missing").to_owned();
        async move { Response::text(200, &format!("pair:{item}:{leaf}")) }
    });
    router.get("/matrix/:item/detail", |req: &Request| {
        let item = req.param("item").unwrap_or("missing").to_owned();
        async move { Response::text(200, &format!("param:{item}")) }
    });
    router.get("/matrix/fixed/detail", |_req: &Request| async {
        Response::text(200, "static")
    });

    router
}

fn generated_route(case: &mut crate::deterministic::DeterministicCase) -> (String, String) {
    let first = CAPTURES[case.bounded(CAPTURE_BOUND)];
    let second = CAPTURES[case.bounded(CAPTURE_BOUND)];

    match case.bounded(ROUTE_MATRIX_BOUND) {
        0 => ("/matrix/fixed/detail".to_owned(), "static".to_owned()),
        1 => (format!("/matrix/{first}/detail"), format!("param:{first}")),
        2 => (
            format!("/matrix/{first}/{second}"),
            format!("pair:{first}:{second}"),
        ),
        3 => (
            format!("/matrix/fixed/{second}"),
            format!("pair:fixed:{second}"),
        ),
        _ => (
            format!("/matrix/{first}/{second}/extra"),
            format!("wildcard:{first}/{second}/extra"),
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_route_precedence_and_capture_matrix_is_stable() {
    let server =
        crate::http::spawn_server_ready(route_matrix_router(), Duration::from_secs(2)).unwrap();
    let generator = crate::deterministic::DeterministicGenerator::stable();

    for index in 0..GENERATED_ROUTE_CASES {
        let mut case = generator.case(index);
        let (path, expected) = generated_route(&mut case);
        let response = http::get(&format!("http://{}{path}", server.local_addr()))
            .await
            .unwrap();

        assert_eq!(response.status(), 200, "{case}: path={path}");
        assert_eq!(response.body(), expected, "{case}: path={path}");
    }

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

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

/// A bare `:` route segment captures under a blank name, and the empty-name
/// query rule does not reach that lookup.
///
/// `parse_segments` reads `:` as a parameter named `""`, so `param("")` answers
/// with the capture — what the released crate returns. Admitting blank query
/// keys put a blank key within reach of a keyed lookup for the first time, and
/// the guard that keeps `query("")` unanswered belongs to the query accessors
/// alone. One request asserts both halves, so a guard that migrates back into
/// the shared pair lookup and silently changes path access fails here.
#[tokio::test(flavor = "multi_thread")]
async fn blank_name_path_param_answers_while_blank_query_name_does_not() {
    let mut router = Router::new();
    router.get("/users/:", |req: &Request| {
        let captured = req.param("").unwrap_or("missing").to_owned();
        let queried = req.query("").unwrap_or("absent").to_owned();
        async move { Response::text(200, &format!("{captured}:{queried}")) }
    });

    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let probe = server.cleanup_probe();

    let response = http::get(&format!("http://{}/users/42?=blank", server.local_addr()))
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        response.body(),
        "42:absent",
        "the bare `:` capture still answers a blank name; the blank query key does not"
    );

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
    assert!(probe.joined(), "path parameter server joined");
    assert_eq!(
        probe.cleanup_error(),
        None,
        "path parameter server cleanup error"
    );
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

#[tokio::test(flavor = "multi_thread")]
async fn structurally_identical_routes_keep_their_own_capture_names() {
    let mut router = Router::new();
    router.get("/accounts/:account_id", |req: &Request| {
        let account_id = req.param("account_id").unwrap_or("missing").to_owned();
        async move { Response::text(200, &account_id) }
    });
    router.post("/accounts/:organization_id", |req: &Request| {
        let organization_id = req.param("organization_id").unwrap_or("missing").to_owned();
        async move { Response::text(200, &organization_id) }
    });

    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let base = format!("http://{}/accounts/acme", server.local_addr());

    assert_eq!(http::get(&base).await.unwrap().body(), "acme");
    assert_eq!(http::post(&base, "").await.unwrap().body(), "acme");

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}
