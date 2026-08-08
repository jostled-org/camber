//! Buffered failures, classified where their producer still knows them.
//!
//! Body collection, JSON parsing, and multipart parsing each answer with their
//! own category and a fixed client-safe message. The parser's own account of
//! what went wrong stays private, and no failure found before the handler
//! reaches it.

use crate::http as wire;
use crate::rejection_support::{
    self as observed, COLLAPSED_STATUS, DECLARED_MESSAGE, Established, Journal, MALFORMED_JSON,
    MALFORMED_MULTIPART, Observed, assert_established,
};
use crate::runtime_support as common;

use camber::RuntimeError;
use camber::http::{HostRouter, RejectionKind, RejectionProtocol, Request, Response, Router};
use camber::runtime;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The body limit every fixture here configures.
const BODY_LIMIT: usize = 1024;

/// The boundary the multipart rows declare.
const MULTIPART_BOUNDARY: &str = "----rejectionboundary";

/// A body larger than [`BODY_LIMIT`] will collect.
fn oversized_body() -> Box<[u8]> {
    vec![b'x'; BODY_LIMIT * 2].into_boxed_slice()
}

/// The request one row sends: where to, as what, and carrying which body.
///
/// Three tables here declare rows, and all three declared this same quartet
/// beside their own claims. Stated once and embedded, because the three
/// selections it drives were also three places the empty-content-type and
/// oversized conventions could come to mean different things.
struct RequestShape {
    path: &'static str,
    /// The representation this row declares, or empty for the multipart one,
    /// whose boundary is only known once [`observed::multipart_content_type`]
    /// has built it.
    content_type: &'static str,
    /// Whether this row sends more bytes than the configured limit collects.
    oversized: bool,
    /// The body this row sends when it is not oversized.
    body: &'static str,
}

/// The two values a shape names that no constant can hold.
///
/// The multipart representation carries a boundary and the oversized body is
/// derived from the configured limit, so both are built once per test and
/// borrowed by every row that names them.
struct RowFixtures {
    multipart_type: Box<str>,
    oversized: Box<[u8]>,
}

impl RowFixtures {
    fn new() -> Self {
        Self {
            multipart_type: observed::multipart_content_type(MULTIPART_BOUNDARY),
            oversized: oversized_body(),
        }
    }

    /// The representation and the bytes one shape actually sends.
    fn sent<'a>(&'a self, shape: &'a RequestShape) -> (&'a str, &'a [u8]) {
        let content_type = match shape.content_type.is_empty() {
            true => self.multipart_type.as_ref(),
            false => shape.content_type,
        };
        let body: &[u8] = match shape.oversized {
            true => &self.oversized,
            false => shape.body.as_bytes(),
        };
        (content_type, body)
    }
}

/// Register the routes every buffered producer in this module is driven through.
///
/// One fixture for the whole table: each route reports that it ran, so a row
/// that claims its failure precedes the handler is proved by the same counter
/// the rows that do reach a handler move.
fn register_buffered_routes(router: &mut Router, handled: &Arc<AtomicUsize>) {
    let uploaded = Arc::clone(handled);
    router.post("/upload", move |_req: &Request| {
        uploaded.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Response::text(200, "stored"))
    });

    // The two parsing terminals are the shared ones, wrapped rather than
    // rewritten: the count is this fixture's own claim and the shared handlers
    // make none, so the increment stays outside and the parse stays theirs.
    let parsed_calls = Arc::clone(handled);
    let ticket = observed::ticket_handler();
    router.post("/parsed", move |req: &Request| {
        parsed_calls.fetch_add(1, Ordering::SeqCst);
        ticket(req)
    });

    let multipart_calls = Arc::clone(handled);
    let counted_parts = observed::multipart_count_handler();
    router.post("/multipart", move |req: &Request| {
        multipart_calls.fetch_add(1, Ordering::SeqCst);
        counted_parts(req)
    });

    let declared_calls = Arc::clone(handled);
    router.post("/declared", move |_req: &Request| {
        declared_calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Err::<Response, RuntimeError>(RuntimeError::BadRequest(
            DECLARED_MESSAGE.into(),
        )))
    });

    let draining_calls = Arc::clone(handled);
    router.post("/draining", move |_req: &Request| {
        draining_calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Err::<Response, RuntimeError>(RuntimeError::ScopeClosed))
    });

    let faulted_calls = Arc::clone(handled);
    router.post("/faulted", move |_req: &Request| {
        faulted_calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Err::<Response, RuntimeError>(RuntimeError::Database(
            PRIVATE_POOL_CAUSE.into(),
        )))
    });
}

/// The private cause the faulted route reports to operators only.
const PRIVATE_POOL_CAUSE: &str = "connection pool exhausted";

/// What one row of the buffered matrix sends, and what it must be answered with.
struct BufferedRow {
    label: &'static str,
    shape: RequestShape,
    kind: RejectionKind,
    status: u16,
    message: &'static str,
    /// Whether this row's producer runs inside the application handler.
    in_handler: bool,
}

const BUFFERED_ROWS: [BufferedRow; 3] = [
    BufferedRow {
        label: "oversized body",
        shape: RequestShape {
            path: "/upload",
            content_type: "application/octet-stream",
            oversized: true,
            body: "",
        },
        kind: RejectionKind::BodyLimit,
        status: 413,
        message: "request body too large",
        in_handler: false,
    },
    BufferedRow {
        label: "malformed json",
        shape: RequestShape {
            path: "/parsed",
            content_type: "application/json",
            oversized: false,
            body: MALFORMED_JSON,
        },
        kind: RejectionKind::MalformedBody,
        status: 400,
        message: "malformed request body",
        in_handler: true,
    },
    BufferedRow {
        label: "malformed multipart",
        shape: RequestShape {
            path: "/multipart",
            content_type: "",
            oversized: false,
            body: MALFORMED_MULTIPART,
        },
        kind: RejectionKind::Multipart,
        status: 400,
        message: "invalid multipart body",
        in_handler: true,
    },
];

/// Assert the context one buffered row's refusal was mapped with.
fn assert_buffered_context(row: &BufferedRow, seen: &Observed) {
    let label = row.label;
    observed::assert_classification(
        seen,
        &observed::Collapsed {
            kind: row.kind,
            status: row.status,
            message: row.message,
        },
        label,
    );
    // Method selection established the route and the dispatch class; no
    // response head ever negotiated a representation.
    assert_established(
        seen,
        &Established {
            method: "POST",
            raw_path: row.shape.path,
            route: Some(row.shape.path),
            protocol: Some(RejectionProtocol::OrdinaryHttp),
            content_type: None,
        },
        label,
    );
}

#[test]
fn buffered_failure_matrix_maps_once_and_never_enters_handler() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new();
            register_buffered_routes(&mut router, &handled);
            let router = router
                .max_request_body(BODY_LIMIT)
                .rejection_mapper(observed::recording_mapper(&journal, "router"));
            let addr = common::spawn_server(router);
            observed::drain(&journal);

            let fixtures = RowFixtures::new();
            let mut expected_handler_calls = 0_usize;
            for row in &BUFFERED_ROWS {
                let (content_type, body) = fixtures.sent(&row.shape);
                let response = wire::send(
                    addr,
                    "POST",
                    row.shape.path,
                    &[("Content-Type", content_type)],
                    body,
                );
                let label = row.label;

                assert_eq!(response.status, row.status, "{label}: wire status");
                assert_eq!(response.text().as_ref(), row.message, "{label}: wire body");
                assert_buffered_context(row, &observed::only(&journal, label));

                expected_handler_calls += usize::from(row.in_handler);
                assert_eq!(
                    handled.load(Ordering::SeqCst),
                    expected_handler_calls,
                    "{label}: handler entry"
                );
            }

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

#[test]
fn json_and_multipart_question_mark_keep_distinct_runtime_provenance() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new();
            register_buffered_routes(&mut router, &handled);
            let router = router
                .max_request_body(BODY_LIMIT)
                .rejection_mapper(observed::recording_mapper(&journal, "router"));
            let addr = common::spawn_server(router);
            observed::drain(&journal);

            let json = wire::send(
                addr,
                "POST",
                "/parsed",
                &[("Content-Type", "application/json")],
                MALFORMED_JSON.as_bytes(),
            );
            let json_seen = observed::only(&journal, "malformed json");
            observed::assert_classification(
                &json_seen,
                &observed::Collapsed {
                    kind: RejectionKind::MalformedBody,
                    status: 400,
                    message: "malformed request body",
                },
                "malformed json",
            );
            assert_eq!(json.text().as_ref(), "malformed request body");
            assert_no_parser_text(&json, &json_seen, "malformed json", JSON_PARSER_TEXT);

            let multipart_type = observed::multipart_content_type(MULTIPART_BOUNDARY);
            let multipart = wire::send(
                addr,
                "POST",
                "/multipart",
                &[("Content-Type", multipart_type.as_ref())],
                MALFORMED_MULTIPART.as_bytes(),
            );
            let multipart_seen = observed::only(&journal, "malformed multipart");
            observed::assert_classification(
                &multipart_seen,
                &observed::Collapsed {
                    kind: RejectionKind::Multipart,
                    status: 400,
                    message: "invalid multipart body",
                },
                "malformed multipart",
            );
            assert_eq!(multipart.text().as_ref(), "invalid multipart body");
            assert_no_parser_text(
                &multipart,
                &multipart_seen,
                "malformed multipart",
                MULTIPART_PARSER_TEXT,
            );

            let declared = wire::send(
                addr,
                "POST",
                "/declared",
                &[("Content-Type", "application/json")],
                b"{}",
            );
            let declared_seen = observed::only(&journal, "declared refusal");
            assert_eq!(
                declared_seen.kind,
                RejectionKind::Application,
                "an application's own refusal is not a parser failure"
            );
            assert_eq!(declared_seen.message.as_ref(), DECLARED_MESSAGE);
            assert_eq!(declared.text().as_ref(), DECLARED_MESSAGE);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// Text only the JSON parser's own account of the failure would carry.
const JSON_PARSER_TEXT: &[&str] = &["expected", "line 1", "EOF", "column"];

/// Text only the multipart parser's own account of the failure would carry.
const MULTIPART_PARSER_TEXT: &[&str] = &["boundary", "delimiter", "framing", "disposition"];

/// Assert a parser's private account reached neither the mapper nor the peer.
///
/// An empty list is refused first, for the reason
/// [`observed::assert_no_private_text`] refuses one: this is a leak assertion,
/// and one handed nothing to look for passes over every answer there is. The
/// peer half is that shared scan; only the mapper half is this root's own, and
/// it runs under the same guard.
fn assert_no_parser_text(
    response: &wire::HttpResponse,
    seen: &Observed,
    label: &str,
    private: &[&str],
) {
    assert!(
        !private.is_empty(),
        "{label}: no parser text was declared, so no leak can be found"
    );
    observed::assert_no_private_text(response, private, label);
    for text in private {
        assert!(
            !seen.message.contains(text),
            "{label}: the parser's private account reached the mapper: {}",
            seen.message
        );
    }
}

/// What one classification row produces, and the category it must keep.
///
/// `private` is the text this row's own producer holds back, and `None` for a
/// producer that holds none. Declared per row rather than as one literal every
/// row is searched for, because a row searched for text its producer never had
/// is a leak assertion that passes over any answer at all.
struct ClassificationRow {
    label: &'static str,
    shape: RequestShape,
    kind: RejectionKind,
    status: u16,
    message: &'static str,
    private: Option<&'static str>,
}

const CLASSIFICATION_ROWS: [ClassificationRow; 6] = [
    ClassificationRow {
        label: "body limit",
        shape: RequestShape {
            path: "/upload",
            content_type: "application/octet-stream",
            oversized: true,
            body: "",
        },
        kind: RejectionKind::BodyLimit,
        status: 413,
        message: "request body too large",
        private: None,
    },
    ClassificationRow {
        label: "malformed body",
        shape: RequestShape {
            path: "/parsed",
            content_type: "application/json",
            oversized: false,
            body: MALFORMED_JSON,
        },
        kind: RejectionKind::MalformedBody,
        status: 400,
        message: "malformed request body",
        private: None,
    },
    ClassificationRow {
        label: "multipart",
        shape: RequestShape {
            path: "/multipart",
            content_type: "",
            oversized: false,
            body: MALFORMED_MULTIPART,
        },
        kind: RejectionKind::Multipart,
        status: 400,
        message: "invalid multipart body",
        private: None,
    },
    ClassificationRow {
        label: "application",
        shape: RequestShape {
            path: "/declared",
            content_type: "application/json",
            oversized: false,
            body: "{}",
        },
        kind: RejectionKind::Application,
        status: 400,
        message: DECLARED_MESSAGE,
        private: None,
    },
    ClassificationRow {
        label: "draining service",
        shape: RequestShape {
            path: "/draining",
            content_type: "application/json",
            oversized: false,
            body: "{}",
        },
        kind: RejectionKind::InternalService,
        status: 503,
        message: observed::UNAVAILABLE_BODY,
        private: None,
    },
    ClassificationRow {
        label: "faulted service",
        shape: RequestShape {
            path: "/faulted",
            content_type: "application/json",
            oversized: false,
            body: "{}",
        },
        kind: RejectionKind::InternalService,
        status: 500,
        message: observed::REDACTED_BODY,
        private: Some(PRIVATE_POOL_CAUSE),
    },
];

#[test]
fn classification_table_keeps_buffered_kinds_distinct() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new();
            register_buffered_routes(&mut router, &handled);
            let router =
                router
                    .max_request_body(BODY_LIMIT)
                    .rejection_mapper(observed::collapsing_mapper(
                        &journal,
                        "router",
                        COLLAPSED_STATUS,
                    ));
            let addr = common::spawn_server(router);
            observed::drain(&journal);

            let fixtures = RowFixtures::new();
            for row in &CLASSIFICATION_ROWS {
                let (content_type, body) = fixtures.sent(&row.shape);
                let response = wire::send(
                    addr,
                    "POST",
                    row.shape.path,
                    &[("Content-Type", content_type)],
                    body,
                );
                let label = row.label;

                observed::assert_collapsed(
                    &journal,
                    &response,
                    label,
                    &observed::Collapsed {
                        kind: row.kind,
                        status: row.status,
                        message: row.message,
                    },
                );
                if let Some(private) = row.private {
                    observed::assert_no_private_text(&response, &[private], label);
                }
            }
            // The field gates the branch above, so its population is part of
            // what this test proves. A table whose rows all read `None` would
            // run the loop, pass every row, and never once look for a leak —
            // the redaction claim would retire silently rather than fail.
            assert_eq!(
                CLASSIFICATION_ROWS
                    .iter()
                    .filter(|row| row.private.is_some())
                    .count(),
                1,
                "exactly one declared row holds a private cause back"
            );
            assert!(
                CLASSIFICATION_ROWS.iter().any(|row| row.private.is_none()),
                "at least one declared row's producer holds nothing back"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Transport disposition ──────────────────────────────────────────

/// What one disposition row sends, and what the transport owes afterwards.
///
/// A refusal raised with the peer's body still unread leaves bytes framed on
/// the connection that nothing will ever read, so nothing establishes where a
/// next request on it would begin. A refusal raised after the whole body was
/// collected leaves the connection exactly as it found it, and saying so is
/// what makes the close a decision rather than a blanket policy.
struct DispositionRow {
    label: &'static str,
    shape: RequestShape,
    /// The category the stage that decided this disposition classified under.
    ///
    /// The disposition is a property of that stage, so a row that read only the
    /// `Connection` value would hold for any refusal that happened to close.
    kind: RejectionKind,
    status: u16,
    /// Whether this refusal must end the connection it was framed on.
    closes: bool,
}

const DISPOSITION_ROWS: [DispositionRow; 2] = [
    DispositionRow {
        label: "unread oversized body",
        shape: RequestShape {
            path: "/upload",
            content_type: "application/octet-stream",
            oversized: true,
            body: "",
        },
        kind: RejectionKind::BodyLimit,
        status: 413,
        closes: true,
    },
    DispositionRow {
        label: "collected malformed body",
        shape: RequestShape {
            path: "/parsed",
            content_type: "application/json",
            oversized: false,
            body: MALFORMED_JSON,
        },
        kind: RejectionKind::MalformedBody,
        status: 400,
        closes: false,
    },
];

/// A body the configured limit collects, so the probe it carries is answerable.
const REUSE_PROBE_BODY: &[u8] = b"probe";

/// Ask a connection that has already been answered to carry one more request.
///
/// `None` is a connection the server ended. The probe reports rather than
/// asserts, because the paused-time case runs it on a thread of its own — see
/// [`spawn_stalled_peer`] for why it cannot be a Tokio task — and a panic on
/// that thread reaches the test as a join failure that has lost what it saw.
fn probe_reuse(stream: &mut TcpStream) -> std::io::Result<Option<wire::HttpResponse>> {
    wire::probe_connection_reuse(
        stream,
        "POST",
        "/upload",
        &[("Content-Type", "application/octet-stream")],
        REUSE_PROBE_BODY,
    )
}

/// Assert what one disposition row's refusal left the connection able to do.
fn assert_disposition(row: &DispositionRow, stream: &mut TcpStream) {
    let label = row.label;
    let answered = probe_reuse(stream)
        .unwrap_or_else(|error| panic!("{label}: the reuse probe did not settle: {error}"));
    match row.closes {
        true => assert!(
            answered.is_none(),
            "{label}: the connection framed another request after a refusal \
             that left the request body unread"
        ),
        false => assert_eq!(
            answered.map(|response| response.status),
            Some(200),
            "{label}: the connection refused another request after a refusal \
             that read the whole body"
        ),
    }
}

#[test]
fn unread_body_refusals_end_the_connection_they_were_framed_on() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new();
            register_buffered_routes(&mut router, &handled);
            let router = router
                .max_request_body(BODY_LIMIT)
                .rejection_mapper(observed::recording_mapper(&journal, "router"));
            let addr = common::spawn_server(router);
            observed::drain(&journal);

            let fixtures = RowFixtures::new();
            for row in &DISPOSITION_ROWS {
                let label = row.label;
                let (content_type, body) = fixtures.sent(&row.shape);

                // The peer offers to keep the connection, so what the response
                // says about it is the framework's decision and not an echo.
                let mut stream = wire::connect(addr)
                    .unwrap_or_else(|error| panic!("{label}: could not connect: {error}"));
                wire::write_request_with_connection(
                    &mut stream,
                    wire::KEEP_CONNECTION,
                    "POST",
                    row.shape.path,
                    &[("Content-Type", content_type)],
                    body,
                )
                .unwrap_or_else(|error| panic!("{label}: could not send: {error}"));
                let refused = wire::read_http_response_bounded(&mut stream)
                    .unwrap_or_else(|error| panic!("{label}: no response: {error}"));

                assert_eq!(refused.status, row.status, "{label}: wire status");
                // Read as the value it is rather than as "not close". A refusal
                // that stated keep-alive and one that stated nothing are two
                // different answers, and a bool collapsing them would hold this
                // row either way.
                let stated = refused.header("connection").map(str::to_ascii_lowercase);
                match row.closes {
                    true => assert_eq!(
                        stated.as_deref(),
                        Some("close"),
                        "{label}: the disposition the producing stage decided"
                    ),
                    false => assert_eq!(
                        stated, None,
                        "{label}: a refusal that read the whole body decides no \
                         disposition, so it states none and the version's own \
                         default stands"
                    ),
                }
                assert_eq!(
                    observed::only(&journal, label).kind,
                    row.kind,
                    "{label}: the stage that decided the disposition"
                );
                assert_disposition(row, &mut stream);
            }

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── A body that stopped being readable ─────────────────────────────

/// The answer a body Camber could not read is given.
const UNREADABLE_BODY_MESSAGE: &str = "request body could not be read";

/// Send one unreadable body, keeping the connection it was framed on.
///
/// The peer offers keep-alive for the reason [`DISPOSITION_ROWS`] does: what the
/// response then says about the connection is the framework's decision and not
/// an echo of the request.
fn send_unreadable_body(addr: SocketAddr) -> (wire::HttpResponse, TcpStream) {
    wire::send_unreadable_body(
        addr,
        wire::KEEP_CONNECTION,
        "POST",
        "/upload",
        "application/octet-stream",
    )
    .unwrap_or_else(|error| panic!("the unreadable body's exchange did not settle: {error}"))
}

/// Assert the context an unreadable body's refusal was mapped with.
fn assert_unreadable_context(seen: &Observed) {
    observed::assert_classification(
        seen,
        &observed::Collapsed {
            kind: RejectionKind::BodyUnreadable,
            status: 400,
            message: UNREADABLE_BODY_MESSAGE,
        },
        "unreadable body",
    );
    // Method selection established the route and the dispatch class; the
    // refusal precedes any response head, so it negotiates no representation.
    assert_established(
        seen,
        &Established {
            method: "POST",
            raw_path: "/upload",
            route: Some("/upload"),
            protocol: Some(RejectionProtocol::OrdinaryHttp),
            content_type: None,
        },
        "unreadable body",
    );
}

/// A body that stopped arriving is not a body that was too large.
///
/// The sibling branch of the same collection failure: `BodyLimit` is the one
/// error the limit owns, and every other collection error is a transport
/// account of a request that was the right size. Answering both as `413` told
/// such a peer to shrink a request nothing had complained about, so this row
/// asserts the category, the status, the safe body, the established context,
/// one mapper call, handler non-entry, and the disposition separately — a row
/// that read only the status would hold for the limit's answer too.
#[test]
fn unreadable_body_refusals_are_not_answered_as_oversized_ones() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new();
            register_buffered_routes(&mut router, &handled);
            let router = router
                .max_request_body(BODY_LIMIT)
                .rejection_mapper(observed::recording_mapper(&journal, "router"));
            let addr = common::spawn_server(router);
            observed::drain(&journal);

            let (refused, mut stream) = send_unreadable_body(addr);
            assert_eq!(refused.status, 400, "wire status");
            assert_eq!(
                refused.text().as_ref(),
                UNREADABLE_BODY_MESSAGE,
                "wire body"
            );
            assert_eq!(
                refused.header("connection").map(str::to_ascii_lowercase),
                Some("close".to_owned()),
                "a refusal that left the body unread ended the connection"
            );

            assert_unreadable_context(&observed::only(&journal, "unreadable body"));
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "a body-collection failure never reaches the application handler"
            );

            // What the peer still owed is unread, so nothing establishes where a
            // next request on this connection would begin.
            let reused = probe_reuse(&mut stream)
                .unwrap_or_else(|error| panic!("the reuse probe did not settle: {error}"));
            assert!(
                reused.is_none(),
                "the connection framed another request after a refusal that left \
                 the request body unread: {reused:?}"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Pre-body policy selection under host routing ───────────────────

/// The authority whose child router configures a rejection policy of its own.
const OWN_POLICY_HOST: &str = "app.test";

/// The authority whose child configures none, so the host's policy answers.
const DELEGATING_HOST: &str = "plain.test";

/// What one host row sends, and whose policy must answer it.
struct HostBodyRow {
    label: &'static str,
    host: &'static str,
    /// The mapper that must have been handed the refusal.
    origin: &'static str,
}

const HOST_BODY_ROWS: [HostBodyRow; 2] = [
    HostBodyRow {
        label: "child with its own policy",
        host: OWN_POLICY_HOST,
        origin: "child",
    },
    HostBodyRow {
        label: "child that configures none",
        host: DELEGATING_HOST,
        origin: "host",
    },
];

/// Serve two children behind one host, one owning a policy and one not.
///
/// The limit is the host's, because a connection reads one body limit; the
/// policy is the child's, because that is the precedence a body-collection
/// refusal has to follow to reach the mapper the route's own failure would.
fn spawn_host_body_fixture(journal: &Journal, handled: &Arc<AtomicUsize>) -> SocketAddr {
    let mut owning = Router::new();
    register_buffered_routes(&mut owning, handled);
    let owning = owning.rejection_mapper(observed::recording_mapper(journal, "child"));

    let mut delegating = Router::new();
    register_buffered_routes(&mut delegating, handled);

    let mut hosts = HostRouter::new()
        .max_request_body(BODY_LIMIT)
        .rejection_mapper(observed::recording_mapper(journal, "host"));
    hosts.add(OWN_POLICY_HOST, owning);
    hosts.add(DELEGATING_HOST, delegating);
    common::spawn_host_server(hosts)
}

#[test]
fn host_routed_body_refusal_answers_under_the_selected_child_policy() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));
            let addr = spawn_host_body_fixture(&journal, &handled);
            observed::drain(&journal);

            let oversized = oversized_body();
            for row in &HOST_BODY_ROWS {
                let label = row.label;
                let refused = wire::request_to_host_with_body(
                    addr,
                    "POST",
                    "/upload",
                    row.host,
                    &[("Content-Type", "application/octet-stream")],
                    &oversized,
                )
                .unwrap_or_else(|error| panic!("{label}: did not complete: {error}"));

                assert_eq!(refused.status, 413, "{label}: wire status");
                let seen = observed::only(&journal, label);
                assert_eq!(
                    seen.origin, row.origin,
                    "{label}: the policy a body refusal was answered under"
                );
                assert_eq!(seen.kind, RejectionKind::BodyLimit, "{label}: category");
                assert_eq!(
                    seen.route.as_deref(),
                    Some("/upload"),
                    "{label}: the resolved child established the route"
                );
                assert_eq!(
                    seen.protocol,
                    Some(RejectionProtocol::OrdinaryHttp),
                    "{label}: the resolved child established the dispatch class"
                );
            }
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "no body-collection refusal reached an application handler"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// Open the stalled connection and send the head that promises more body than
/// it sends.
///
/// The peer offers to keep the connection for the reason [`DISPOSITION_ROWS`]
/// does: what the refusal then says about it is the framework's decision and
/// not an echo of the request.
fn open_stalled(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let mut peer = wire::connect(addr)?;
    wire::write_stalled_body(&mut peer, Some(wire::KEEP_CONNECTION), "POST", "/upload")?;
    Ok(peer)
}

/// What the stalled peer was answered, and what its connection did next.
struct StalledExchange {
    refusal: wire::HttpResponse,
    /// The answer to one further request, or `None` from a connection the
    /// server ended.
    reused: Option<wire::HttpResponse>,
}

/// Read the one frame the refusal was, then ask that connection for one more.
///
/// One frame, and none of what may follow it: a read to end of stream returns
/// only once the server has already closed, which would leave the reuse probe
/// unable to fail however the framework had behaved.
fn stalled_exchange(peer: &mut TcpStream) -> std::io::Result<StalledExchange> {
    let refusal = wire::read_http_response_bounded(peer)?;
    let reused = probe_reuse(peer)?;
    Ok(StalledExchange { refusal, reused })
}

/// What the stalled peer reports: that its head is sent, then what it was
/// answered.
type StalledReports = (
    tokio::sync::oneshot::Receiver<std::io::Result<()>>,
    tokio::sync::oneshot::Receiver<std::io::Result<StalledExchange>>,
);

/// Run the stalled peer on a thread of its own, reporting each half as it
/// settles.
///
/// The peer is the suite's own blocking socket, bounded by the real-time
/// deadline [`wire::connect`] arms on it. Nothing here arms a Tokio timer: with
/// the clock paused the runtime advances to the earliest armed timer whenever
/// it has nothing left to run, so a Tokio bound armed by this test would be the
/// earliest timer for as long as the server has none of its own, and would
/// expire while the server was still waiting for work rather than failing to
/// answer. `spawn_blocking` cannot host the peer either: a Tokio blocking task
/// inhibits that advance for as long as it runs, and the advance is what
/// reaches the collection deadline this case is about. A thread outside the
/// runtime leaves the clock free and still carries a real bound.
///
/// The caller starts serving only once the head is sent, so no accepted
/// connection ever waits for a request head: such a connection waits on Hyper's
/// own header deadline, and a paused clock would reach that deadline instead of
/// the collection one.
fn spawn_stalled_peer(addr: SocketAddr) -> (std::thread::JoinHandle<()>, StalledReports) {
    let (sent_tx, sent_rx) = tokio::sync::oneshot::channel();
    let (answered_tx, answered_rx) = tokio::sync::oneshot::channel();
    let peer = std::thread::spawn(move || match open_stalled(addr) {
        Ok(mut peer) => {
            // A oneshot send fails only when its receiver is gone, which here
            // means the test has already failed and is unwinding past both
            // reports. Nothing is left to tell, so the report is discarded.
            let _ = sent_tx.send(Ok(()));
            // Discarded for the same reason, and with more at stake: the peer
            // must still finish so its socket closes rather than holding the
            // unwinding test's connection open.
            let _ = answered_tx.send(stalled_exchange(&mut peer));
        }
        // Nothing was sent, so nothing can be answered: the exchange sender
        // drops here, and the caller's first report is the failure itself.
        Err(error) => {
            let _ = sent_tx.send(Err(error));
        }
    });
    (peer, (sent_rx, answered_rx))
}

/// The outer bound this fixture's teardown runs under.
///
/// Well past anything a shutdown of one idle connection waits on, so it never
/// displaces a join that was going to arrive.
const TEARDOWN_BOUND: Duration = Duration::from_secs(300);

/// How many times that bound is armed before an unjoined server is a failure.
///
/// More than one, for the reason [`wire::bounded_under_pause`] states: a paused
/// clock advances to the nearest armed timer whenever the runtime idles, so an
/// arming made before the shutdown has a deadline of its own is the only timer
/// in the process and elapses having proved nothing.
const TEARDOWN_ARMINGS: usize = 4;

/// Assert the context a stalled body's deadline refusal was mapped with.
///
/// The sibling of [`assert_unreadable_context`], and stated the same way: method
/// selection established the route and the dispatch class before the deadline
/// was armed, so a refusal raised while the body was still owed still names
/// both. A row that read only the status would hold for either sibling.
fn assert_timeout_context(seen: &Observed) {
    observed::assert_classification(
        seen,
        &observed::Collapsed {
            kind: RejectionKind::BodyTimeout,
            status: 408,
            message: "request body timed out",
        },
        "body timeout",
    );
    assert_eq!(seen.route.as_deref(), Some("/upload"), "established route");
    assert_eq!(
        seen.protocol,
        Some(RejectionProtocol::OrdinaryHttp),
        "established dispatch class"
    );
}

/// The collection deadline runs on virtual time, and the handler never sees it.
///
/// This row is declared by 3.T1 but cannot run inside the shared fixture
/// runtime, which owns a live clock; paused time needs a runtime of its own, so
/// the row is its own test rather than a case of the buffered matrix.
///
/// The peer sends a head that promises a body and then one byte of it, offering
/// to keep the connection afterwards. Nothing waits on a wall clock: the runtime
/// is paused, so the deadline is reached by the scheduler running out of work
/// rather than by elapsed real time. The peer's own bounds are real, and only a
/// failure ever reaches them — see [`spawn_stalled_peer`] for why they cannot be
/// Tokio's.
///
/// Every other test in this root serves through `spawn_server`, and the runtime
/// scope releases that server when `RuntimeBuilder::run` unwinds. This one holds
/// the listener itself — it must start serving only after the stalled head is
/// sent — so [`wire::ReadyServer`] owns the handle instead, and an assertion that
/// fails before the join below still cancels the server task and closes the
/// listener under it. `Drop` cannot await, and this runtime is current-thread
/// with its clock paused, so cancellation is the whole of what the unwind path
/// owes; the ordinary path below joins.
#[tokio::test(start_paused = true)]
async fn body_collection_deadline_maps_before_the_handler() {
    let _context = camber::runtime_test_support::install_runtime_context();
    let journal = Journal::default();
    let handled = Arc::new(AtomicUsize::new(0));

    let mut router = Router::new();
    register_buffered_routes(&mut router, &handled);
    let router = router.rejection_mapper(observed::recording_mapper(&journal, "router"));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|error| panic!("the stalled fixture reserved a port: {error}"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|error| panic!("the reserved port reported its address: {error}"));
    let (peer, (sent, answered)) = spawn_stalled_peer(addr);
    sent.await
        .expect("the stalled peer reported its head")
        .expect("the stalled head was sent");

    let server = wire::serve_owned(listener, |listener| {
        camber::http::serve_background(listener, router)
    })
    .unwrap_or_else(|error| panic!("the stalled fixture's server took its listener: {error}"));
    let exchange = answered
        .await
        .expect("the stalled peer reported what it was answered")
        .expect("the stalled peer's exchange settled");
    peer.join().expect("the stalled peer ran to completion");

    let refusal = &exchange.refusal;
    assert_eq!(refusal.status, 408, "the stalled body's wire status");
    assert_eq!(
        refusal.text().as_ref(),
        "request body timed out",
        "the stalled body's answer"
    );
    assert_eq!(
        refusal.header("Connection"),
        Some("close"),
        "a deadline that left the body unread ended the connection"
    );

    // What the peer still owed is unread, so nothing establishes where a next
    // request on this connection would begin.
    assert!(
        exchange.reused.is_none(),
        "the connection answered a second request after a refusal that left the body unread: {:?}",
        exchange.reused
    );

    assert_timeout_context(&observed::only(&journal, "body timeout"));
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "a body-collection failure never reaches the application handler"
    );

    // Bounded, because the clock is paused: an unbounded join on a supervisor
    // that never returns parks this whole binary instead of failing here.
    wire::bounded_under_pause(
        server.into_handle().shutdown_and_join(),
        TEARDOWN_BOUND,
        TEARDOWN_ARMINGS,
        "the stalled fixture's server",
    )
    .await
    .expect("the server joined");
}
