//! The public `Router::multipart` journey: what a handler is given, what the
//! finalizer answers with, and what the buffered API keeps.
//!
//! Every case here sends a real request over a real socket and reads the answer
//! off it. The lifecycle observer counts what the production driver and the
//! production permit owner did; it decides nothing.

use crate::http as wire;
use crate::rejection_support::{
    Journal, assert_no_private_text, drain, journal, only, recording_mapper,
};
use crate::streaming_multipart::{
    ABSENT, BOUNDARY, Escape, Escapes, Field, HandlerFuture, assert_escaped_inert, content_type,
    declining_reader, drain_field, escaping_handler, failing_handler, field_opening,
    multipart_body, read_fields, reading_handler, silent_handler, upload,
};

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, HostRouter, Method, MultipartField, MultipartLimits,
    MultipartStream, RejectionKind, Request, Response, Router,
};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long any fixture here has to answer, connect, pause, or shut down.
const BOUND: Duration = Duration::from_secs(5);

/// The maximum one field may carry in the cases that are about field bounds.
const FIELD_BYTES: usize = 512;

/// The maximum one delivered chunk may carry.
const CHUNK_BYTES: usize = 64;

/// The limits every case here registers.
fn limits() -> MultipartLimits {
    MultipartLimits::builder()
        .max_fields(8)
        .max_field_bytes(FIELD_BYTES)
        .max_headers_per_field(8)
        .max_header_bytes_per_field(1024)
        .max_chunk_bytes(CHUNK_BYTES)
        .max_parser_buffer_bytes(4096)
        .build()
        .expect("the fixture limits are a valid combination")
}

/// The headers one multipart request sends.
fn declared() -> Box<str> {
    content_type(BOUNDARY)
}

/// A handler that discards every field named `skip` and reads the rest.
fn discarding_handler(_request: &Request, mut stream: MultipartStream) -> HandlerFuture {
    Box::pin(async move {
        let mut described: Vec<String> = Vec::new();
        while let Some(field) = stream.next_field().await? {
            described.push(skip_or_read(field).await?);
        }
        Response::text(200, &described.join(","))
    })
}

/// Discard the field named `skip`, and read every other one to its end.
async fn skip_or_read(mut field: MultipartField<'_>) -> Result<String, RuntimeError> {
    let name = field.name().to_owned();
    match name.as_str() {
        "skip" => field.discard().await.map(|()| format!("{name}/discarded")),
        _ => Ok(format!("{name}/{}", drain_field(&mut field).await?)),
    }
}

/// The router every reading case is served through.
fn reading_router(handled: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    router.multipart(
        Method::Post,
        "/upload",
        limits(),
        reading_handler("read", "absent", handled),
    );
    router.multipart(Method::Post, "/skip", limits(), discarding_handler);
    router
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_multipart_route_yields_large_repeated_fields_under_both_limits() {
    let handled = Arc::new(AtomicUsize::new(0));
    let server = wire::spawn_server_ready(reading_router(&handled).max_request_body(8192), BOUND)
        .expect("the reading fixture answered");
    let addr = server.local_addr();

    let ordered = multipart_body(
        BOUNDARY,
        &[
            Field::text("note", "hello"),
            Field::file("upload", "a.bin", "application/octet-stream", &[3u8; 200]),
        ],
    );
    let answered = upload(addr, "/upload", &ordered);
    assert_eq!(answered.status, 200, "an admitted body reaches its handler");
    assert_eq!(
        String::from_utf8_lossy(&answered.body),
        format!("read|{ABSENT}|note/-/-/5/1,upload/a.bin/application/octet-stream/200/4"),
        "fields arrive in wire order, and a large field arrives in right-sized chunks"
    );

    let repeated = multipart_body(
        BOUNDARY,
        &[
            Field::text("part", "aa"),
            Field::text("part", "bbb"),
            Field::text("empty", ""),
        ],
    );
    let answered = upload(addr, "/upload", &repeated);
    assert_eq!(
        String::from_utf8_lossy(&answered.body),
        format!("read|{ABSENT}|part/-/-/2/1,part/-/-/3/1,empty/-/-/0/0"),
        "repeated names are preserved in order, and an empty field yields no chunk"
    );

    let skipped = multipart_body(
        BOUNDARY,
        &[
            Field::text("keep", "kept"),
            Field::bytes("skip", &[9u8; 300]),
            Field::text("after", "last"),
        ],
    );
    let answered = upload(addr, "/skip", &skipped);
    assert_eq!(
        (answered.status, String::from_utf8_lossy(&answered.body)),
        (200, "keep/4,skip/discarded,after/4".into()),
        "a discarded field completes that field and permits exactly the next one"
    );

    let oversize_field = multipart_body(BOUNDARY, &[Field::bytes("big", &[1u8; FIELD_BYTES + 8])]);
    let answered = upload(addr, "/upload", &oversize_field);
    assert_eq!(
        answered.status, 413,
        "a field larger than its configured maximum is refused"
    );

    let over_ceiling = multipart_body(BOUNDARY, &[Field::bytes("big", &[1u8; 9000])]);
    let answered = upload(addr, "/upload", &over_ceiling);
    assert_eq!(
        answered.status, 413,
        "a representation larger than the route admits is refused"
    );
    server.shutdown_bounded(BOUND).expect("the fixture stopped");

    assert_exact_ceiling(&ordered);
    assert_host_ceiling_narrows(&ordered);
}

/// A body whose length is exactly the effective maximum is admitted.
fn assert_exact_ceiling(body: &[u8]) {
    let handled = Arc::new(AtomicUsize::new(0));
    let server =
        wire::spawn_server_ready(reading_router(&handled).max_request_body(body.len()), BOUND)
            .expect("the exact-ceiling fixture answered");
    let answered = upload(server.local_addr(), "/upload", body);
    assert_eq!(
        answered.status, 200,
        "a body exactly at the effective maximum is admitted"
    );
    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}

/// A host ceiling narrows a child that configured a larger one.
fn assert_host_ceiling_narrows(body: &[u8]) {
    let handled = Arc::new(AtomicUsize::new(0));
    let mut hosts = HostRouter::new();
    hosts.add(
        "narrow.test",
        reading_router(&handled).max_request_body(64 * 1024),
    );
    let hosts = hosts.max_request_body(body.len() - 1);
    let port = wire::reserve_observed();
    let server = port.serve_hosts(hosts);

    let declared = declared();
    let answered = wire::request_to_host_with_body(
        server.addr(),
        "POST",
        "/upload",
        "narrow.test",
        &[("Content-Type", declared.as_ref())],
        body,
    )
    .expect("the host fixture answered");
    assert_eq!(
        answered.status, 413,
        "the host ceiling narrows the child that configured a larger one"
    );
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "a refused body reaches no handler"
    );
}

/// Every report the catching handler made about reading its own body, in order.
type Reports = Arc<Mutex<Vec<bool>>>;

/// A handler that swallows whatever reading its body returned, and reports
/// whether there was anything to swallow.
///
/// The report is what its row reads the claim off. The answer on the wire is the
/// refusal's — a caught failure outranks the handler's success, so the handler's
/// own response is discarded — and an observation cannot travel in a body
/// nothing sends.
fn catching_handler(
    reports: &Reports,
) -> impl Fn(&Request, MultipartStream) -> HandlerFuture + Send + Sync + 'static {
    let reports = Arc::clone(reports);
    move |_request: &Request, stream: MultipartStream| {
        let reports = Arc::clone(&reports);
        Box::pin(async move {
            let caught = read_fields(stream).await.is_err();
            reports
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(caught);
            Response::text(200, &format!("caught={caught}"))
        })
    }
}

/// Take what the catching handler has reported since the last row.
fn caught_since(reports: &Reports) -> Vec<bool> {
    std::mem::take(&mut *reports.lock().unwrap_or_else(|error| error.into_inner()))
}

/// One failure row: what was sent where, and how it must be classified.
struct Failure {
    label: &'static str,
    path: &'static str,
    body: FailureBody,
    status: u16,
    kind: RejectionKind,
    /// What the catching handler must have reported by the time this row is
    /// answered. `None` is a row that never reaches it.
    caught: Option<bool>,
}

/// What one failure row puts on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailureBody {
    /// A field larger than the route's per-field maximum.
    OversizeField,
    /// A valid body, framed chunked, that runs past the admitted total.
    ChunkedOverTotal,
    /// An opening delimiter that is not this request's boundary.
    WrongDelimiter,
    /// A valid body with its closing delimiter cut off.
    Truncated,
    /// A part that declares a nested multipart representation.
    Nested,
    /// A chunked body whose framing stops being readable.
    Unreadable,
    /// A valid, complete body.
    Valid,
}

/// The complete failure table.
const FAILURES: &[Failure] = &[
    Failure {
        label: "a field past its configured maximum is a byte limit",
        path: "/upload",
        body: FailureBody::OversizeField,
        status: 413,
        kind: RejectionKind::BodyLimit,
        caught: None,
    },
    Failure {
        label: "a representation past the admitted total is a byte limit",
        path: "/upload",
        body: FailureBody::ChunkedOverTotal,
        status: 413,
        kind: RejectionKind::BodyLimit,
        caught: None,
    },
    Failure {
        label: "a delimiter that frames nothing is a multipart failure",
        path: "/upload",
        body: FailureBody::WrongDelimiter,
        status: 400,
        kind: RejectionKind::Multipart,
        caught: None,
    },
    Failure {
        label: "a body that ends before its closing delimiter is a multipart failure",
        path: "/upload",
        body: FailureBody::Truncated,
        status: 400,
        kind: RejectionKind::Multipart,
        caught: None,
    },
    Failure {
        label: "a nested multipart part is a multipart failure",
        path: "/upload",
        body: FailureBody::Nested,
        status: 400,
        kind: RejectionKind::Multipart,
        caught: None,
    },
    Failure {
        label: "a transport that stops delivering is unreadable",
        path: "/upload",
        body: FailureBody::Unreadable,
        status: 400,
        kind: RejectionKind::BodyUnreadable,
        caught: None,
    },
    Failure {
        label: "a handler that declines keeps its own category",
        path: "/decline",
        body: FailureBody::Valid,
        status: 400,
        kind: RejectionKind::Application,
        caught: None,
    },
    Failure {
        label: "a caught parser failure still outranks the handler's success",
        path: "/catch",
        body: FailureBody::WrongDelimiter,
        status: 400,
        kind: RejectionKind::Multipart,
        caught: Some(true),
    },
    Failure {
        label: "a handler that read nothing cannot commit its success",
        path: "/silent",
        body: FailureBody::Valid,
        status: 400,
        kind: RejectionKind::Multipart,
        caught: None,
    },
    Failure {
        label: "a handler that declines after reading keeps its own category",
        path: "/read-then-decline",
        body: FailureBody::Valid,
        status: 400,
        kind: RejectionKind::Application,
        caught: None,
    },
];

/// The private text no failure answer may carry.
const PRIVATE: &[&str] = &[
    "delimiter",
    "configured maximum",
    "closing",
    "admitted maximum",
    "chunk",
];

/// Send one failure row and read the answer off the wire.
fn drive_failure(addr: std::net::SocketAddr, row: &Failure) -> wire::HttpResponse {
    match row.body {
        FailureBody::OversizeField => upload(
            addr,
            row.path,
            &multipart_body(BOUNDARY, &[Field::bytes("big", &[2u8; FIELD_BYTES + 64])]),
        ),
        FailureBody::ChunkedOverTotal => send_chunked_multipart(addr, row.path, 40),
        FailureBody::WrongDelimiter => upload(
            addr,
            row.path,
            &multipart_body("NotTheBoundary", &[Field::text("a", "b")]),
        ),
        FailureBody::Truncated => upload(addr, row.path, &truncated_body()),
        FailureBody::Nested => upload(
            addr,
            row.path,
            &multipart_body(
                BOUNDARY,
                &[Field::file(
                    "nested",
                    "inner",
                    "multipart/mixed; boundary=inner",
                    b"data",
                )],
            ),
        ),
        FailureBody::Unreadable => {
            let declared = declared();
            let (answered, _socket) =
                wire::send_unreadable_body(addr, "close", "POST", row.path, declared.as_ref())
                    .expect("the unreadable body was answered");
            answered
        }
        FailureBody::Valid => upload(
            addr,
            row.path,
            &multipart_body(BOUNDARY, &[Field::text("note", "hello")]),
        ),
    }
}

/// A valid body with its closing delimiter cut off.
fn truncated_body() -> Vec<u8> {
    let mut body = multipart_body(BOUNDARY, &[Field::text("note", "hello")]);
    body.truncate(body.len() - (BOUNDARY.len() + 6));
    body
}

/// Send one chunked multipart body of `chunks` frames, each 256 bytes of one
/// field's data.
///
/// Chunked because a declared length past the maximum is refused before a frame
/// is polled: the only way to reach the observed-byte bound is to declare
/// nothing.
fn send_chunked_multipart(
    addr: std::net::SocketAddr,
    path: &str,
    chunks: usize,
) -> wire::HttpResponse {
    let declared = declared();
    let mut socket = wire::connect(addr).expect("the chunked fixture connected");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
         Content-Type: {declared}\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    socket
        .write_all(head.as_bytes())
        .and_then(|()| socket.flush())
        .expect("the chunked head was sent");
    let opening = field_opening(BOUNDARY, &Field::bytes("big", b""));
    let frames = wire::tolerate_answered_peer(write_chunked_body(&mut socket, &opening, chunks));
    frames.expect("the chunked frames were sent or the peer had already answered");
    wire::read_http_response_bounded(&mut socket).expect("the chunked request was answered")
}

/// Write one chunked multipart body: its opening delimiter, then `chunks` data
/// frames the route must measure against its admitted total.
fn write_chunked_body(
    socket: &mut std::net::TcpStream,
    opening: &str,
    chunks: usize,
) -> std::io::Result<()> {
    wire::write_chunk(socket, opening.as_bytes())?;
    for _ in 0..chunks {
        wire::write_chunk(socket, &[4u8; 256])?;
    }
    wire::write_chunked_end(socket)
}

/// What one handler-error row leaves the connection it was framed on able to do.
///
/// The two rows differ in one thing: whether the handler read its payload before
/// it declined. A clean terminal answers a handler failure with the handler's
/// category and nothing else, while an abandoned one adds the unread-body
/// disposition — and a connection that must survive the answer and one that must
/// not are what tell those two apart on the wire.
struct Disposition {
    label: &'static str,
    path: &'static str,
    /// Whether this answer must end the connection it was framed on.
    closes: bool,
}

/// Both handler-error rows of the precedence table.
const DISPOSITIONS: &[Disposition] = &[
    Disposition {
        label: "a handler that declined after reading leaves the connection framed",
        path: "/read-then-decline",
        closes: false,
    },
    Disposition {
        label: "a handler that declined without reading ends the connection",
        path: "/decline",
        closes: true,
    },
];

/// Send one handler-error row on a connection the peer offered to keep, and
/// assert what the answer decided about it.
fn assert_disposition(addr: std::net::SocketAddr, row: &Disposition, mapped: &Journal) {
    let label = row.label;
    let declared = declared();
    let mut socket =
        wire::connect(addr).unwrap_or_else(|error| panic!("{label}: could not connect: {error}"));
    wire::write_request_with_connection(
        &mut socket,
        wire::KEEP_CONNECTION,
        "POST",
        row.path,
        &[("Content-Type", declared.as_ref())],
        &multipart_body(BOUNDARY, &[Field::text("note", "hello")]),
    )
    .unwrap_or_else(|error| panic!("{label}: could not send: {error}"));
    let answered = wire::read_http_response_bounded(&mut socket)
        .unwrap_or_else(|error| panic!("{label}: no response: {error}"));

    assert_eq!(answered.status, 400, "{label}: wire status");
    let observed = only(mapped, label);
    assert_eq!(
        (observed.origin, observed.kind),
        ("route", RejectionKind::Application),
        "{label}: the handler's own category answered once: {observed:?}"
    );
    // Read as the value it is rather than as "not close": an answer that stated
    // keep-alive and one that stated nothing are two different answers, and a
    // bool collapsing them would hold this row either way.
    let stated = answered.header("connection").map(str::to_ascii_lowercase);
    assert_eq!(
        stated.as_deref(),
        row.closes.then_some("close"),
        "{label}: the disposition this terminal decided"
    );
    assert_reuse(&mut socket, row);
}

/// Assert what the connection could still carry after that answer.
fn assert_reuse(socket: &mut std::net::TcpStream, row: &Disposition) {
    let label = row.label;
    let declared = declared();
    let probed = wire::probe_connection_reuse(
        socket,
        "POST",
        "/upload",
        &[("Content-Type", declared.as_ref())],
        &multipart_body(BOUNDARY, &[Field::text("note", "hello")]),
    )
    .unwrap_or_else(|error| panic!("{label}: the reuse probe did not settle: {error}"));
    match row.closes {
        true => assert!(
            probed.is_none(),
            "{label}: the connection framed another request behind payload nothing read"
        ),
        false => assert_eq!(
            probed.map(|response| response.status),
            Some(200),
            "{label}: the connection refused another request after an answer that \
             left no payload unread"
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_multipart_failure_categories_map_once() {
    let handled = Arc::new(AtomicUsize::new(0));
    let mapped = journal();
    let caught: Reports = Arc::new(Mutex::new(Vec::new()));
    let mut router = reading_router(&handled);
    router.multipart(Method::Post, "/decline", limits(), failing_handler);
    router.multipart(Method::Post, "/catch", limits(), catching_handler(&caught));
    router.multipart(Method::Post, "/silent", limits(), silent_handler);
    router.multipart(
        Method::Post,
        "/read-then-decline",
        limits(),
        declining_reader,
    );
    let router = router
        .max_request_body(4096)
        .rejection_mapper(recording_mapper(&mapped, "route"));
    let server =
        wire::spawn_server_ready(router, BOUND).expect("the failure fixture answered its probe");
    let addr = server.local_addr();

    for row in FAILURES {
        let answered = drive_failure(addr, row);
        assert_eq!(
            answered.status, row.status,
            "{}: answered with {}",
            row.label, answered.status
        );
        let observed = only(&mapped, row.label);
        assert_eq!(
            (observed.origin, observed.kind),
            ("route", row.kind),
            "{}: classified once by the route's own mapper: {observed:?}",
            row.label
        );
        assert_eq!(
            observed.route.as_deref(),
            Some(row.path),
            "{}: the refusal names the route that answered: {observed:?}",
            row.label
        );
        assert_no_private_text(&answered, PRIVATE, row.label);
        assert_eq!(
            caught_since(&caught),
            row.caught.map_or_else(Vec::new, |report| vec![report]),
            "{}: what the catching handler reported about reading its own body",
            row.label
        );
    }

    for row in DISPOSITIONS {
        assert_disposition(addr, row, &mapped);
    }

    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}

/// What one finalizer fixture's router is wired with, and what its rows read
/// their claims off.
struct Wiring {
    escapes: Arc<Escapes>,
    handled: Arc<AtomicUsize>,
    /// Counts the admitted permit's own drop, once per session.
    released: Arc<AtomicUsize>,
    mapped: Journal,
}

impl Wiring {
    /// A fixture with nothing recorded yet.
    fn new() -> Self {
        Self {
            escapes: Escapes::new(),
            handled: Arc::new(AtomicUsize::new(0)),
            released: Arc::new(AtomicUsize::new(0)),
            mapped: journal(),
        }
    }
}

/// The router every finalizer and escaped-handle row is served through.
///
/// One listener for all of them, because the counters each row states its claim
/// against are the listener's: a row that stood up its own server would be
/// reading a session's numbers against a fixture rather than against what the
/// rows before it left behind.
fn finalizer_router(wiring: &Wiring) -> Router {
    let permits = Arc::clone(&wiring.released);
    let mut router = Router::new();
    router.multipart(
        Method::Post,
        "/read",
        limits(),
        reading_handler("read", "absent", &wiring.handled),
    );
    router.multipart(
        Method::Post,
        "/read-then-decline",
        limits(),
        declining_reader,
    );
    router.multipart(
        Method::Post,
        "/channel",
        limits(),
        escaping_handler(Escape::Channel, &wiring.escapes),
    );
    router.multipart(
        Method::Post,
        "/task",
        limits(),
        escaping_handler(Escape::Task, &wiring.escapes),
    );
    router
        .body_admission(move |_context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                4096,
                wire::permit_probe(&permits),
            ))
        })
        .rejection_mapper(recording_mapper(&wiring.mapped, "route"))
}

/// One finalizer row: what the peer sends, and what the answer must be once the
/// session has released everything it owned.
struct Finalizer {
    label: &'static str,
    path: &'static str,
    body: FinalizerBody,
    status: u16,
    /// The category the selected mapper must record. `None` is the row the
    /// handler's own success answers.
    kind: Option<RejectionKind>,
}

/// What one finalizer row puts on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalizerBody {
    /// A valid, complete body.
    Valid,
    /// An opening delimiter that is not this request's boundary.
    WrongDelimiter,
    /// A field larger than the route's per-field maximum.
    OversizeField,
}

impl FinalizerBody {
    /// Frame this row's body exactly as its peer sends it.
    fn wire(self) -> Vec<u8> {
        match self {
            Self::Valid => multipart_body(BOUNDARY, &[Field::text("note", "hello")]),
            Self::WrongDelimiter => multipart_body("NotTheBoundary", &[Field::text("a", "b")]),
            Self::OversizeField => {
                multipart_body(BOUNDARY, &[Field::bytes("big", &[2u8; FIELD_BYTES + 64])])
            }
        }
    }
}

/// Every precedence row this layer reaches without a transport fault.
///
/// A clean terminal answered by the handler's success and by its error, a parser
/// failure and a byte-limit refusal that outrank the handler either way, and —
/// in the escaped-handle rows below — an abandoned terminal under a handler
/// success. The abandoned-plus-error row is driven by
/// `streaming_multipart_failure_categories_map_once`. Disconnect, reset,
/// shutdown, and cancellation are the live matrix's, in 3.T4.
const FINALIZERS: &[Finalizer] = &[
    Finalizer {
        label: "a clean terminal commits the handler's own response",
        path: "/read",
        body: FinalizerBody::Valid,
        status: 200,
        kind: None,
    },
    Finalizer {
        label: "a parser failure is mapped over a handler that carried it out",
        path: "/read",
        body: FinalizerBody::WrongDelimiter,
        status: 400,
        kind: Some(RejectionKind::Multipart),
    },
    Finalizer {
        label: "a byte-limit refusal is mapped over a handler that carried it out",
        path: "/read",
        body: FinalizerBody::OversizeField,
        status: 413,
        kind: Some(RejectionKind::BodyLimit),
    },
    Finalizer {
        label: "a handler error over a clean terminal keeps the handler's category",
        path: "/read-then-decline",
        body: FinalizerBody::Valid,
        status: 400,
        kind: Some(RejectionKind::Application),
    },
];

#[tokio::test(flavor = "multi_thread")]
async fn multipart_finalizer_revokes_escaped_handles_before_response_selection() {
    let wiring = Wiring::new();
    let port = wire::reserve_observed();
    let controller = port.controller();
    let server = port.serve(finalizer_router(&wiring));
    let addr = server.addr();

    for row in FINALIZERS {
        assert_finalizer_row(row, addr, &wiring, &controller).await;
    }

    for (path, escape) in [("/channel", Escape::Channel), ("/task", Escape::Task)] {
        assert_escape_row(
            &Escaping {
                addr,
                path,
                escape,
                escapes: Arc::clone(&wiring.escapes),
                released: Arc::clone(&wiring.released),
                mapped: Arc::clone(&wiring.mapped),
            },
            &controller,
        )
        .await;
    }
}

/// Drive one finalizer row and assert what its session had already released
/// when its response was selected.
///
/// The claim is the same for every row and it is about ownership rather than
/// about the answer: whatever terminal the session reached, revocation ran, the
/// driver returned, and the admitted permit was gone before precedence chose
/// anything. The answer is asserted afterwards, so a row cannot pass by being
/// refused for a reason other than its own.
async fn assert_finalizer_row(
    row: &Finalizer,
    addr: std::net::SocketAddr,
    wiring: &Wiring,
    controller: &Arc<LifecycleController>,
) {
    let checkpoint = LifecycleCheckpoint::BeforeMultipartResponseSelection;
    let label = row.label;
    let before = controller.multipart_observed();
    let permits_before = wiring.released.load(Ordering::SeqCst);
    controller
        .pause_once(checkpoint)
        .expect("the response-selection checkpoint armed");
    let (path, body) = (row.path, row.body.wire());
    let sending = tokio::task::spawn_blocking(move || upload(addr, path, &body));
    tokio::time::timeout(BOUND, controller.wait_until_paused(checkpoint))
        .await
        .unwrap_or_else(|_| panic!("{label}: the response-selection checkpoint was reached"))
        .expect("the checkpoint is armed");

    let observed = controller.multipart_observed();
    assert_eq!(
        (
            observed.revocations(),
            observed.drivers_terminated(),
            observed.permit_owners_dropped()
        ),
        (
            before.revocations() + 1,
            before.drivers_terminated() + 1,
            before.permit_owners_dropped() + 1
        ),
        "{label}: revocation, driver termination, and permit release precede \
         response selection: {observed:?}"
    );
    assert_eq!(
        wiring.released.load(Ordering::SeqCst),
        permits_before + 1,
        "{label}: the admitted permit dropped exactly once"
    );
    controller
        .release(checkpoint)
        .expect("the checkpoint released");

    let answered = sending.await.expect("the request task finished");
    assert_eq!(
        answered.status, row.status,
        "{label}: answered with {}",
        answered.status
    );
    assert_finalizer_mapping(row, &wiring.mapped);
}

/// Assert one finalizer row's answer was classified once, or not at all.
fn assert_finalizer_mapping(row: &Finalizer, mapped: &Journal) {
    let label = row.label;
    match row.kind {
        Some(kind) => {
            let observed = only(mapped, label);
            assert_eq!(
                (observed.origin, observed.kind),
                ("route", kind),
                "{label}: the route's own mapper classified it once: {observed:?}"
            );
        }
        None => {
            let observed = drain(mapped);
            assert!(
                observed.is_empty(),
                "{label}: nothing was refused, so no mapper ran: {observed:?}"
            );
        }
    }
}

/// One escaped-handle row: where the handler leaves its handle, and what the
/// case reads its claims off.
struct Escaping {
    addr: std::net::SocketAddr,
    path: &'static str,
    escape: Escape,
    escapes: Arc<Escapes>,
    released: Arc<AtomicUsize>,
    mapped: Journal,
}

/// Drive one escaped-handle row and assert what was true before its response
/// was selected.
async fn assert_escape_row(row: &Escaping, controller: &Arc<LifecycleController>) {
    let checkpoint = LifecycleCheckpoint::BeforeMultipartResponseSelection;
    let path = row.path;
    // The counters belong to the listener and both rows run against it, so each
    // row states its claim against what it found rather than against a number it
    // assumed the row before had left behind.
    let before = controller.multipart_observed();
    let permits_before = row.released.load(Ordering::SeqCst);
    controller
        .pause_once(checkpoint)
        .expect("the response-selection checkpoint armed");
    let addr = row.addr;
    let sending = tokio::task::spawn_blocking(move || {
        upload(
            addr,
            path,
            &multipart_body(BOUNDARY, &[Field::text("note", "hello")]),
        )
    });
    tokio::time::timeout(BOUND, controller.wait_until_paused(checkpoint))
        .await
        .expect("the response-selection checkpoint was reached")
        .expect("the checkpoint is armed");

    // Held before the response is selected: revocation has run, the driver has
    // returned, and the permit owner is already gone.
    let observed = controller.multipart_observed();
    assert_eq!(
        (observed.revocations(), observed.drivers_terminated()),
        (before.revocations() + 1, before.drivers_terminated() + 1),
        "{path}: revocation and driver termination precede response selection: {observed:?}"
    );
    assert_eq!(
        row.released.load(Ordering::SeqCst),
        permits_before + 1,
        "{path}: the admitted permit is released before the response is selected"
    );

    assert_escaped_inert(
        &row.escapes,
        row.escape,
        observed.body_frames_polled(),
        controller,
        path,
        BOUND,
    )
    .await;
    controller
        .release(checkpoint)
        .expect("the checkpoint released");

    let answered = sending.await.expect("the request task finished");
    assert_eq!(
        answered.status, 400,
        "{path}: a handler that handed its stream away read nothing"
    );
    let mapped = only(&row.mapped, path);
    assert_eq!(
        (mapped.origin, mapped.kind),
        ("route", RejectionKind::Multipart),
        "{path}: an incomplete body is classified once: {mapped:?}"
    );
}

/// A handler that answers with what the buffered reader materialized.
fn buffered_handler(request: &Request) -> HandlerFuture {
    let parsed = request.multipart().map(|reader| {
        reader
            .parts()
            .iter()
            .map(|part| {
                format!(
                    "{}/{}/{}/{}",
                    part.name(),
                    part.filename().unwrap_or(ABSENT),
                    part.content_type().unwrap_or(ABSENT),
                    part.data().len()
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    });
    Box::pin(async move { Response::text(200, &parsed?) })
}

#[tokio::test(flavor = "multi_thread")]
async fn buffered_multipart_and_ordinary_routes_keep_their_contracts() {
    let handled = Arc::new(AtomicUsize::new(0));
    let mapped = journal();
    let mut router = reading_router(&handled);
    router.post("/buffered", buffered_handler);
    router.post("/json", |request: &Request| {
        let body: Box<str> = request.body().into();
        async move { Response::text(200, &format!("json {body}")) }
    });
    let router = router
        .max_request_body(8192)
        .rejection_mapper(recording_mapper(&mapped, "route"));
    let server =
        wire::spawn_server_ready(router, BOUND).expect("the compatibility fixture answered");
    let addr = server.local_addr();

    let body = multipart_body(
        BOUNDARY,
        &[
            Field::text("note", "hello"),
            Field::file("upload", "a.bin", "application/octet-stream", &[5u8; 120]),
        ],
    );
    let buffered = upload(addr, "/buffered", &body);
    assert_eq!(
        (buffered.status, String::from_utf8_lossy(&buffered.body)),
        (
            200,
            "note/-/-/5,upload/a.bin/application/octet-stream/120".into()
        ),
        "the materialized reader keeps its parts, filenames, and representations"
    );

    let streamed = upload(addr, "/upload", &body);
    assert_eq!(
        String::from_utf8_lossy(&streamed.body),
        format!("read|{ABSENT}|note/-/-/5/1,upload/a.bin/application/octet-stream/120/2"),
        "the streaming route answers the same body one bounded chunk at a time"
    );

    let ordinary = wire::request_to_host_with_body(
        addr,
        "POST",
        "/json",
        "localhost",
        &[("Content-Type", "application/json")],
        b"{\"id\":1}",
    )
    .expect("the ordinary route answered");
    assert_eq!(
        (ordinary.status, String::from_utf8_lossy(&ordinary.body)),
        (200, "json {\"id\":1}".into()),
        "an ordinary route still collects its whole body before its handler runs"
    );

    let malformed = upload(
        addr,
        "/buffered",
        &multipart_body("NotTheBoundary", &[Field::text("a", "b")]),
    );
    assert_eq!(
        malformed.status, 400,
        "the buffered parser keeps refusing what it always refused"
    );
    let observed = only(&mapped, "buffered multipart refusal");
    assert_eq!(
        (observed.origin, observed.kind),
        ("route", RejectionKind::Multipart),
        "the buffered refusal keeps its own provenance: {observed:?}"
    );

    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}
