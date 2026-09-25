use crate::runtime_support as common;

use crate::deterministic::{DeterministicCase, DeterministicGenerator};
use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::time::Duration;

const GENERATED_ROUTE_CASES: u64 = 48;
const ROUTE_MATRIX_BOUND: NonZeroUsize = NonZeroUsize::new(5).unwrap();
const CAPTURE_BOUND: NonZeroUsize = NonZeroUsize::new(4).unwrap();
const CAPTURES: [&str; 4] = ["alpha", "bravo", "charlie", "delta"];

/// The checked-in seed every escaped route case derives from.
const ROUTE_ESCAPE_SEED: u64 = 0x524f_5554_4500_000a;
const GENERATED_ESCAPE_CASES: u64 = 48;

/// Path spellings the client sends unchanged and the router must keep.
///
/// Routing reads the accepted path, never a decoded one, so each capture is
/// expected in exactly this spelling. No fragment spells a dot segment, which
/// the client would normalize before sending.
const PATH_ESCAPES: [(&str, &str); 10] = [
    ("escaped-space", "%20"),
    ("escaped-slash-upper", "%2F"),
    ("escaped-slash-lower", "%2f"),
    ("escaped-percent", "%25"),
    ("utf8-escape", "%E2%9C%93"),
    ("malformed-escape", "%zz"),
    ("plus", "+"),
    ("sub-delims", "!$&'()*,;="),
    ("colon-at", ":@"),
    ("tilde", "~"),
];

/// Escaped spellings of the static segment `fixed`. Each must miss the static
/// route and fall to the parameter route with its spelling intact.
const ESCAPED_STATIC: [&str; 3] = ["%66ixed", "fixe%64", "%66%69%78%65%64"];

/// The route shapes each escaped case draws from. A run must reach every one,
/// or the escape table proves less than it claims.
const ESCAPED_ROUTE_SHAPES: [&str; 6] = [
    "escaped-param",
    "escaped-pair",
    "escaped-static-spelling",
    "escaped-slash-one-segment",
    "escaped-static-then-param",
    "escaped-wildcard",
];

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

fn generated_route(case: &mut DeterministicCase) -> (String, String) {
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

/// One escaped capture: a plain name with one or two escape fragments, and
/// the table label of each fragment.
///
/// The plain prefix keeps every capture distinct from the static `fixed`
/// segment and from a dot segment.
fn escaped_capture(case: &mut DeterministicCase) -> (Box<str>, Box<[&'static str]>) {
    let mut capture = CAPTURES[case.bounded(CAPTURE_BOUND)].to_owned();
    let fragments = 1 + usize::from(case.boolean());
    let mut labels = Vec::with_capacity(fragments);
    for _ in 0..fragments {
        let (label, spelling) = *case.pick(&PATH_ESCAPES);
        capture.push_str(spelling);
        labels.push(label);
    }
    (capture.into_boxed_str(), labels.into_boxed_slice())
}

/// A generated route case: its category, the path sent, the body its route
/// must answer, and every table label it reached.
#[derive(Debug, PartialEq)]
struct GeneratedRoute {
    category: &'static str,
    path: Box<str>,
    expected: Box<str>,
    labels: Box<[&'static str]>,
}

/// A case from the precedence matrix, whose captures carry no escapes.
fn generated_precedence_route(case: &mut DeterministicCase) -> GeneratedRoute {
    let (path, expected) = generated_route(case);
    GeneratedRoute {
        category: "precedence",
        path: path.into(),
        expected: expected.into(),
        labels: Box::new([]),
    }
}

/// One escaped route shape, and the escape labels of the captures its path
/// actually sends.
///
/// Both captures are drawn on every case so the generated stream stays fixed,
/// but a capture the shape leaves out never reaches the wire and proves none
/// of its escapes.
fn escaped_route_target(
    case: &mut DeterministicCase,
    labels: &mut Vec<&'static str>,
) -> (&'static str, String, String) {
    let (first, first_labels) = escaped_capture(case);
    let (second, second_labels) = escaped_capture(case);

    let shape = *case.pick(&ESCAPED_ROUTE_SHAPES);
    let (path, expected, sends_first, sends_second) = match shape {
        "escaped-param" => (
            format!("/matrix/{first}/detail"),
            format!("param:{first}"),
            true,
            false,
        ),
        "escaped-pair" => (
            format!("/matrix/{first}/{second}"),
            format!("pair:{first}:{second}"),
            true,
            true,
        ),
        "escaped-static-spelling" => {
            let spelling = *case.pick(&ESCAPED_STATIC);
            (
                format!("/matrix/{spelling}/detail"),
                format!("param:{spelling}"),
                false,
                false,
            )
        }
        "escaped-slash-one-segment" => (
            format!("/matrix/{first}%2F{second}/detail"),
            format!("param:{first}%2F{second}"),
            true,
            true,
        ),
        "escaped-static-then-param" => (
            format!("/matrix/fixed/{second}"),
            format!("pair:fixed:{second}"),
            false,
            true,
        ),
        "escaped-wildcard" => (
            format!("/matrix/{first}/{second}/extra"),
            format!("wildcard:{first}/{second}/extra"),
            true,
            true,
        ),
        unlisted => panic!("{case}: {unlisted} has no escaped route"),
    };
    if sends_first {
        labels.extend(first_labels);
    }
    if sends_second {
        labels.extend(second_labels);
    }
    (shape, path, expected)
}

fn generated_escaped_route(case: &mut DeterministicCase) -> GeneratedRoute {
    let mut labels = Vec::new();
    let (shape, path, expected) = escaped_route_target(case, &mut labels);
    labels.push(shape);
    GeneratedRoute {
        category: shape,
        path: path.into(),
        expected: expected.into(),
        labels: labels.into_boxed_slice(),
    }
}

/// Sends one generated path to the shared server and checks the route that
/// answered it.
async fn assert_route_answers(
    server: &crate::http::ReadyServer,
    case: &DeterministicCase,
    route: &GeneratedRoute,
) {
    let context = format!("{case} category={} path={}", route.category, route.path);
    let response = http::get(&format!("http://{}{}", server.local_addr(), route.path))
        .await
        .unwrap();

    assert_eq!(response.status(), 200, "{context}");
    assert_eq!(response.body(), route.expected.as_ref(), "{context}");
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_route_precedence_and_capture_matrix_is_stable() {
    let server =
        crate::http::spawn_server_ready(route_matrix_router(), Duration::from_secs(2)).unwrap();
    let generator = DeterministicGenerator::stable();

    for index in 0..GENERATED_ROUTE_CASES {
        let (case, route) = generator.reproducible(index, generated_precedence_route);
        assert_route_answers(&server, &case, &route).await;
    }

    let escapes = DeterministicGenerator::new(ROUTE_ESCAPE_SEED);
    let mut reached = BTreeSet::new();
    for index in 0..GENERATED_ESCAPE_CASES {
        let (case, route) = escapes.reproducible(index, generated_escaped_route);
        assert_eq!(case.seed(), ROUTE_ESCAPE_SEED, "{case}: checked-in seed");
        assert_route_answers(&server, &case, &route).await;
        reached.extend(route.labels.iter().copied());
    }

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();

    escapes.assert_reached(
        GENERATED_ESCAPE_CASES,
        PATH_ESCAPES
            .iter()
            .map(|(label, _)| *label)
            .chain(ESCAPED_ROUTE_SHAPES),
        &reached,
    );
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
