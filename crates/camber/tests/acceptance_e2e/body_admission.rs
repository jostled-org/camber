//! Route-resolved body limits, read off the wire a peer actually sent.
//!
//! Every claim here is a claim about framing, transport disposition, or
//! ownership across a real connection: what a declaration alone is answered
//! with, which of Hyper and Camber refused a row, what a connection does with
//! unread payload behind it, and when the admitted permit is released. Counters
//! contribute the internal counts a wire cannot show; they never stand in for
//! the wire itself.

use crate::common;
use crate::http as wire;

use camber::RuntimeError;
use camber::http::mock::ScopedRequestBodyOwner;
use camber::http::{BodyAdmission, BodyAdmissionContext, RejectionKind, Request, Response, Router};
use camber::runtime;
use common::{Journal, assert_no_private_text, drain, only, recording_mapper};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The configured ceiling every case here runs its plain route under.
const CEILING: usize = 64;
/// The narrower maximum the policy selects for its own route.
const SELECTED_MAX: usize = 16;
/// The largest declaration Hyper decodes and hands to Camber.
///
/// Hyper's own decoded-length ceiling is two below `u64::MAX`, so this is the
/// widest declaration a Camber limit is ever asked about.
const WIDEST_ADMITTED_DECLARATION: &str = "18446744073709551613";
/// A declared length Hyper refuses to decode at all.
const UNDECODABLE_DECLARATION: &str = "18446744073709551615";
/// A declaration far above any route ceiling that an HTTP/2 peer still carries.
///
/// Wider than a 32-bit word and four billion times the ceiling below, but well
/// inside what the HTTP/2 stack will frame. [`UNDECODABLE_DECLARATION`] is
/// twenty digits, which is one more than the h2 framing layer will parse, so
/// that extreme is refused below Camber on both transports and each has a row
/// naming its owner.
const OVERSIZE_H2_DECLARATION: &str = "4294967296";

/// The route whose ceiling is the router's own.
const PLAIN_ROUTE: &str = "/plain/:id";
/// The target every framing row asks for.
const FRAME_TARGET: &str = "/plain/frame";
/// The route whose policy narrows the ceiling and hands over a permit.
const SELECTED_ROUTE: &str = "/selected/:id";

/// How long a release or a live handshake has to settle.
const SETTLE_BOUND: Duration = Duration::from_secs(5);

/// A permit whose release is the only thing it records.
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// A handler that reports it ran and echoes exactly what it was given.
fn counting_echo(
    handled: &Arc<AtomicUsize>,
) -> impl Fn(&Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static {
    let handled = Arc::clone(handled);
    move |request: &Request| {
        handled.fetch_add(1, Ordering::SeqCst);
        let body: Box<str> = request.body().into();
        Box::pin(async move { Response::text(200, &body).expect("valid echo status") })
    }
}

/// A policy that narrows one route's maximum and hands it a permit.
fn narrowing_policy(
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(calls);
    let drops = Arc::clone(drops);
    move |context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        match context.route() == SELECTED_ROUTE {
            true => Ok(BodyAdmission::with_permit(
                SELECTED_MAX,
                DropProbe(Arc::clone(&drops)),
            )),
            false => Ok(BodyAdmission::new(usize::MAX)),
        }
    }
}

/// The two routes, the ceiling, the policy, and the recording mapper every case
/// here shares.
fn limited_server(
    handled: &Arc<AtomicUsize>,
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
    mapped: &Journal,
) -> wire::ObservedServer<ScopedRequestBodyOwner> {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.post(PLAIN_ROUTE, counting_echo(handled));
    router.post(SELECTED_ROUTE, counting_echo(handled));
    port.serve(
        router
            .max_request_body(CEILING)
            .body_admission(narrowing_policy(calls, drops))
            .rejection_mapper(recording_mapper(mapped, "router")),
    )
}

/// How many refusals the mapper has recorded.
fn mapped_count(mapped: &Journal) -> usize {
    mapped
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .len()
}

/// Assert a release count settles at exactly `expected`.
fn assert_released(drops: &Arc<AtomicUsize>, expected: usize, label: &str) {
    let settled = wire::poll_until(SETTLE_BOUND, || drops.load(Ordering::Acquire) == expected);
    assert!(
        settled,
        "{label}: expected {expected} releases, saw {}",
        drops.load(Ordering::Acquire)
    );
}

// ── A declaration alone is enough to refuse ──────────────────────────

/// Refuse one withheld declaration and prove the connection cannot frame
/// anything after it.
///
/// The peer asks for keep-alive, so the close the server applies is the
/// framework's own decision and not an echo of a preference.
fn assert_withheld_declaration_closes(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    path: &str,
    declared: &str,
    label: &str,
) {
    let (refused, mut peer) = wire::send_withheld_declaration(
        server.addr(),
        wire::KEEP_CONNECTION,
        "POST",
        path,
        declared,
    )
    .expect("the withheld declaration was answered");
    assert_eq!(
        refused.status, 413,
        "{label}: the declaration alone is refused"
    );
    let reused = wire::probe_connection_reuse(&mut peer, "POST", "/plain/reuse", &[], b"probe");
    assert!(
        matches!(reused, Ok(None) | Err(_)),
        "{label}: unread payload must not leave the connection able to frame another request: {reused:?}"
    );
}

/// Assert the declaration Hyper will not decode is refused below Camber.
///
/// `u64::MAX` is past Hyper's own decoded-length ceiling, so no request head is
/// ever constructed and there is no Camber identity, policy call, or mapped
/// rejection to claim for it. Stated here rather than assumed, because the
/// boundary is the claim.
fn assert_undecodable_declaration_is_hypers(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    mapped: &Journal,
) {
    let before = mapped_count(mapped);
    let (refused, mut peer) = wire::send_withheld_declaration(
        server.addr(),
        wire::KEEP_CONNECTION,
        "POST",
        "/plain/undecodable",
        UNDECODABLE_DECLARATION,
    )
    .expect("the undecodable declaration was answered");
    assert_eq!(
        refused.status, 431,
        "a declaration above Hyper's decoded-length ceiling is refused below Camber"
    );
    assert_eq!(
        mapped_count(mapped),
        before,
        "a refusal below Camber claims no mapped rejection"
    );
    let reused = wire::probe_connection_reuse(&mut peer, "POST", "/plain/reuse", &[], b"probe");
    assert!(
        matches!(reused, Ok(None) | Err(_)),
        "the parser's own refusal ends the connection too: {reused:?}"
    );
}

#[test]
fn withheld_declared_oversize_refuses_and_closes_http1_before_arrival() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let server = limited_server(&handled, &calls, &drops, &mapped);

            assert_withheld_declaration_closes(&server, "/plain/1", "100000", "configured ceiling");
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "the ceiling refuses before the policy"
            );

            assert_withheld_declaration_closes(
                &server,
                "/plain/2",
                WIDEST_ADMITTED_DECLARATION,
                "widest declaration Hyper decodes",
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);

            assert_undecodable_declaration_is_hypers(&server, &mapped);

            assert_withheld_declaration_closes(
                &server,
                "/selected/3",
                "32",
                "route-selected maximum",
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the selected row asked the policy once"
            );
            assert_released(
                &drops,
                1,
                "selected-limit refusal releases the permit it was handed",
            );

            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "no withheld row reached a handler"
            );
            assert_eq!(
                server.controller().observed().frames_polled,
                0,
                "a declaration is refused before the first body poll"
            );
            assert_eq!(server.controller().observed().peak_retained_bytes, 0);
            assert_eq!(
                mapped_count(&mapped),
                3,
                "each Camber-owned refusal was mapped exactly once"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Who refused which framing ────────────────────────────────────────

/// Which of the two parsers a framing row is answered by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Owner {
    /// Hyper refused below Camber: no request head was ever constructed, so
    /// there is no Camber identity, policy call, mapper call, or handler entry
    /// to claim.
    Hyper,
    /// Camber answered: the route was matched, its policy ran, and its mapper
    /// saw exactly one refusal.
    Camber,
}

/// What a row's answer left the connection able to do.
///
/// Every row asks its peer to keep the connection, so a row that can frame
/// nothing afterwards was closed by the server's own decision rather than by the
/// preference it was sent with — the confound
/// [`assert_withheld_declaration_closes`] avoids the same way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Disposition {
    /// The exchange finished and another request can be framed behind it.
    Reusable,
    /// Nothing can be framed after this answer.
    Closed,
}

/// One raw framing row and the answer it must get.
struct FramingRow {
    label: &'static str,
    request: Box<str>,
    status: u16,
    owner: Owner,
    /// How many times this row reached the configured body policy. A Camber
    /// refusal decided before the policy — the configured ceiling, and a coding
    /// Camber cannot decode — never asks the application about a request its own
    /// framing rules have already ruled out.
    policy: usize,
    /// The category Camber classified this row under, when Camber owns it and
    /// the row is a refusal.
    kind: Option<RejectionKind>,
    /// What this row leaves the connection able to do. A refusal decided before
    /// the framed body was read leaves bytes nothing establishes the end of, so
    /// the connection cannot carry another request.
    disposition: Disposition,
}

/// Send exact request bytes and read whatever comes back.
///
/// The connection is handed back rather than dropped: what a row leaves the
/// connection able to do is half of its claim, and a peer closed here would
/// answer that question by ending it.
fn raw_exchange(
    addr: std::net::SocketAddr,
    request: &str,
) -> (wire::HttpResponse, std::net::TcpStream) {
    use std::io::Write as _;
    let mut peer = wire::connect(addr).expect("the framing peer connected");
    peer.write_all(request.as_bytes())
        .expect("the framing bytes reached the server");
    peer.flush().expect("the framing bytes were flushed");
    let answered =
        wire::read_http_response_bounded(&mut peer).expect("the framing row was answered");
    (answered, peer)
}

/// One request head with the caller's exact framing lines, and its body bytes.
fn framed(framing: &str, body: &str) -> Box<str> {
    let keep = wire::KEEP_CONNECTION;
    format!(
        "POST {FRAME_TARGET} HTTP/1.1\r\nHost: localhost\r\nConnection: {keep}\r\n{framing}\r\n{body}"
    )
    .into_boxed_str()
}

/// The rows Hyper answers on its own, below any Camber policy.
fn hyper_owned_rows() -> Box<[FramingRow]> {
    let rows = [
        (
            "conflicting duplicate lengths",
            "Content-Length: 5\r\nContent-Length: 6\r\n",
            "",
        ),
        ("malformed length", "Content-Length: abc\r\n", ""),
        (
            "overflowing length",
            "Content-Length: 99999999999999999999999\r\n",
            "",
        ),
        ("prefixed length", "Content-Length: +5\r\n", ""),
        (
            "non-terminal transfer coding",
            "Transfer-Encoding: chunked, gzip\r\n",
            "0\r\n\r\n",
        ),
    ];
    rows.into_iter()
        .map(|(label, framing, body)| FramingRow {
            label,
            request: framed(framing, body),
            status: 400,
            owner: Owner::Hyper,
            policy: 0,
            kind: None,
            disposition: Disposition::Closed,
        })
        .chain([FramingRow {
            label: "chunked framing on HTTP/1.0",
            request: format!(
                "POST {FRAME_TARGET} HTTP/1.0\r\nHost: localhost\r\nConnection: {}\r\n\
                 Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
                wire::KEEP_CONNECTION
            )
            .into_boxed_str(),
            status: 400,
            owner: Owner::Hyper,
            policy: 0,
            kind: None,
            disposition: Disposition::Closed,
        }])
        .collect()
}

/// The rows Camber answers, having matched a route and run its policy.
fn camber_owned_rows() -> Box<[FramingRow]> {
    Box::new([
        FramingRow {
            label: "absent length",
            request: framed("", ""),
            status: 200,
            owner: Owner::Camber,
            policy: 1,
            kind: None,
            disposition: Disposition::Reusable,
        },
        FramingRow {
            label: "one valid declaration",
            request: framed("Content-Length: 5\r\n", "hello"),
            status: 200,
            owner: Owner::Camber,
            policy: 1,
            kind: None,
            disposition: Disposition::Reusable,
        },
        FramingRow {
            label: "equal duplicate lengths",
            request: framed("Content-Length: 5\r\nContent-Length: 5\r\n", "hello"),
            status: 200,
            owner: Owner::Camber,
            policy: 1,
            kind: None,
            disposition: Disposition::Reusable,
        },
        FramingRow {
            label: "chunked",
            request: framed("Transfer-Encoding: chunked\r\n", "5\r\nhello\r\n0\r\n\r\n"),
            status: 200,
            owner: Owner::Camber,
            policy: 1,
            kind: None,
            disposition: Disposition::Reusable,
        },
        FramingRow {
            label: "length beside transfer coding",
            request: framed(
                "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n",
                "5\r\nhello\r\n0\r\n\r\n",
            ),
            status: 200,
            owner: Owner::Camber,
            policy: 1,
            kind: None,
            // Admitted and served, and the parser still refuses to carry
            // anything more: a peer that framed one body two ways is one an
            // intermediary could read a second request out of, so Hyper drops
            // the declaration it ignored and ends the connection after the
            // answer. The disposition here is the parser's, not the limit's.
            disposition: Disposition::Closed,
        },
        FramingRow {
            label: "accepted coding Camber cannot decode",
            request: framed(
                "Transfer-Encoding: gzip, chunked\r\n",
                "5\r\nhello\r\n0\r\n\r\n",
            ),
            status: 400,
            owner: Owner::Camber,
            policy: 0,
            kind: Some(RejectionKind::MalformedBody),
            // The chunked framing under the coding was never decoded, so nothing
            // establishes where the bytes behind it stop being this body.
            disposition: Disposition::Closed,
        },
        FramingRow {
            label: "declaration above the ceiling",
            request: framed(&format!("Content-Length: {}\r\n", CEILING + 1), ""),
            status: 413,
            owner: Owner::Camber,
            policy: 0,
            kind: Some(RejectionKind::BodyLimit),
            disposition: Disposition::Closed,
        },
    ])
}

/// Assert one framing row's answer and the observations its owner implies.
fn assert_framing_row(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    row: &FramingRow,
    calls: &Arc<AtomicUsize>,
    handled: &Arc<AtomicUsize>,
    mapped: &Journal,
) {
    let policy_before = calls.load(Ordering::SeqCst);
    let handled_before = handled.load(Ordering::SeqCst);
    let mapped_before = mapped_count(mapped);
    let (answered, mut peer) = raw_exchange(server.addr(), &row.request);
    assert_eq!(
        answered.status, row.status,
        "{}: unexpected status",
        row.label
    );

    let policy = calls.load(Ordering::SeqCst) - policy_before;
    let entered = handled.load(Ordering::SeqCst) - handled_before;
    let refusals = mapped_count(mapped) - mapped_before;
    match row.owner {
        Owner::Hyper => assert_eq!(
            (policy, entered, refusals),
            (0, 0, 0),
            "{}: a row refused below Camber claims no policy, handler, or mapped rejection",
            row.label
        ),
        Owner::Camber => assert_eq!(
            policy, row.policy,
            "{}: unexpected number of body-policy calls",
            row.label
        ),
    }
    assert_camber_row_outcome(row, entered, refusals, mapped);
    assert_disposition(&mut peer, row);
}

/// Assert what one row's answer left the connection able to do.
///
/// Asserted last, because the probe is itself a request: every row reads its own
/// counters before this runs, so whatever the probe is served lands outside all
/// of them.
fn assert_disposition(peer: &mut std::net::TcpStream, row: &FramingRow) {
    let reused = wire::probe_connection_reuse(peer, "POST", "/plain/reuse", &[], b"probe");
    match row.disposition {
        Disposition::Closed => assert!(
            matches!(reused, Ok(None) | Err(_)),
            "{}: an unread body must not leave the connection able to frame another request: {reused:?}",
            row.label
        ),
        Disposition::Reusable => {
            let answered = reused
                .unwrap_or_else(|error| panic!("{}: the connection failed: {error}", row.label))
                .unwrap_or_else(|| {
                    panic!("{}: the connection framed no second request", row.label)
                });
            assert_eq!(
                answered.status, 200,
                "{}: the second request was answered",
                row.label
            );
            assert_eq!(
                answered.text().as_ref(),
                "probe",
                "{}: the second request was served, not just answered",
                row.label
            );
        }
    }
}

/// Assert what a Camber-owned row did beyond its policy call.
fn assert_camber_row_outcome(row: &FramingRow, entered: usize, refusals: usize, mapped: &Journal) {
    match (row.owner, row.kind) {
        (Owner::Hyper, _) => {}
        (Owner::Camber, None) => {
            assert_eq!(
                entered, 1,
                "{}: an admitted row reaches its handler",
                row.label
            );
            assert_eq!(
                refusals, 0,
                "{}: an admitted row maps no refusal",
                row.label
            );
        }
        (Owner::Camber, Some(kind)) => {
            assert_eq!(
                entered, 0,
                "{}: a refused row reaches no handler",
                row.label
            );
            assert_eq!(
                refusals, 1,
                "{}: a refused row maps exactly once",
                row.label
            );
            assert_last_mapped(mapped, kind, PLAIN_ROUTE, FRAME_TARGET, row.label);
        }
    }
}

/// Assert the refusal the mapper recorded last is the one this row provoked.
///
/// Four claims and one subject: the category the producer classified it under,
/// the route its limit was selected from, the target that names which request it
/// was, and a well-shaped identifier an operator can correlate it against. A
/// category alone would be satisfied by any earlier row's record.
///
/// Read under the lock rather than copied out: the journal entry is the
/// mapper's own record, and nothing here needs a second owner of it.
fn assert_last_mapped(
    mapped: &Journal,
    kind: RejectionKind,
    route: &str,
    raw_path: &str,
    label: &str,
) {
    let journal = mapped.lock().unwrap_or_else(|error| error.into_inner());
    let seen = journal
        .last()
        .unwrap_or_else(|| panic!("{label}: the mapper recorded nothing"));
    assert_eq!(seen.kind, kind, "{label}: unexpected category");
    assert_eq!(seen.route.as_deref(), Some(route), "{label}: route");
    assert_eq!(seen.raw_path.as_ref(), raw_path, "{label}: target");
    common::assert_request_id_shape(Some(seen.request_id.as_ref()), label);
}

#[test]
fn http1_framing_matrix_names_hyper_and_camber_enforcement_owners() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let server = limited_server(&handled, &calls, &drops, &mapped);

            let rows: Box<[FramingRow]> = hyper_owned_rows()
                .into_iter()
                .chain(camber_owned_rows())
                .collect();
            rows.iter()
                .for_each(|row| assert_framing_row(&server, row, &calls, &handled, &mapped));

            assert_eq!(
                drops.load(Ordering::Acquire),
                0,
                "the plain route's policy hands over no permit"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

// ── A crossing frame stops the read and the connection ───────────────

/// Open a chunked request and pace its frames one at a time.
///
/// The body is deliberately left unterminated: what this case reads back is the
/// refusal the crossing frame produced, and a peer that had already finished its
/// body would leave nothing unread to decide a disposition over.
fn pace_chunks(
    peer: &mut std::net::TcpStream,
    path: &str,
    chunks: &[&[u8]],
) -> std::io::Result<()> {
    wire::write_chunked_head(
        peer,
        wire::KEEP_CONNECTION,
        "POST",
        path,
        wire::DEFAULT_HOST,
    )?;
    chunks
        .iter()
        .try_for_each(|chunk| wire::write_chunk(peer, chunk))
}

#[test]
fn live_chunked_buffered_crossing_never_reaches_handler() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let server = limited_server(&handled, &calls, &drops, &mapped);

            let under = [b'u'; CEILING - 4];
            let mut peer = wire::connect(server.addr()).expect("the paced peer connected");
            wire::tolerate_dead_socket(pace_chunks(
                &mut peer,
                "/plain/crossing",
                &[&under, b"crossing", b"never"],
            ))
            .expect("the paced frames reached the server");
            let refused =
                wire::read_http_response_bounded(&mut peer).expect("the crossing was answered");

            assert_eq!(
                refused.status, 413,
                "the crossing frame produces the limit refusal"
            );
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "a crossing body reaches no handler"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the policy ran once for this request"
            );
            let seen = only(&mapped, "chunked crossing");
            assert_eq!(seen.kind, RejectionKind::BodyLimit);
            assert_eq!(seen.route.as_deref(), Some(PLAIN_ROUTE));
            assert!(
                server.controller().observed().frames_polled >= 2,
                "the frames before the crossing were polled and counted"
            );
            assert_eq!(
                server.controller().observed().peak_retained_bytes,
                CEILING - 4,
                "the crossing frame was measured before it could be retained"
            );

            let reused = wire::probe_connection_reuse(&mut peer, "POST", "/plain/reuse", &[], b"x");
            assert!(
                matches!(reused, Ok(None) | Err(_)),
                "unread chunked framing must not leave the connection reusable: {reused:?}"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Disconnect and cancellation release the same one owner ───────────

/// The route whose handler holds its admitted request until the peer is gone.
const HELD_ROUTE: &str = "/held/:id";

/// Serve a route that reports handler entry and then waits to be cancelled.
fn holding_server(
    entered: std::sync::mpsc::SyncSender<()>,
    drops: &Arc<AtomicUsize>,
) -> wire::ObservedServer<ScopedRequestBodyOwner> {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.post(HELD_ROUTE, move |_request: &Request| {
        let entered = entered.clone();
        Box::pin(async move {
            entered
                .send(())
                .expect("the case is waiting for handler entry");
            // Never resolves. The only way out is the request future being
            // dropped, which is the cancellation this row is about.
            std::future::pending::<()>().await;
            Response::text(200, "unreachable").expect("valid held status")
        }) as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    let drops = Arc::clone(drops);
    port.serve(router.max_request_body(CEILING).body_admission(
        move |_context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                CEILING,
                DropProbe(Arc::clone(&drops)),
            ))
        },
    ))
}

/// Disconnect while the body is still arriving, and prove one release.
fn assert_disconnect_during_collection(drops: &Arc<AtomicUsize>) {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    let handled = Arc::new(AtomicUsize::new(0));
    router.post(PLAIN_ROUTE, counting_echo(&handled));
    let probe = Arc::clone(drops);
    let server = port.serve(router.max_request_body(CEILING).body_admission(
        move |_context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                CEILING,
                DropProbe(Arc::clone(&probe)),
            ))
        },
    ));

    let mut peer = wire::connect(server.addr()).expect("the disconnecting peer connected");
    wire::write_stalled_body(
        &mut peer,
        Some(wire::KEEP_CONNECTION),
        "POST",
        "/plain/gone",
    )
    .expect("the promise of a body reached the server");
    drop(peer);
    assert_released(drops, 1, "disconnect during collection");
    assert_eq!(
        handled.load(Ordering::SeqCst),
        0,
        "an abandoned body reaches no handler"
    );
    assert_eq!(
        server.controller().observed().permit_owners_dropped,
        1,
        "the listener counted the same single release"
    );
}

#[test]
fn buffered_disconnect_and_request_cancellation_release_permit_once() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let drops = Arc::new(AtomicUsize::new(0));
            assert_disconnect_during_collection(&drops);

            let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
            let cancelled = Arc::new(AtomicUsize::new(0));
            let server = holding_server(entered_tx, &cancelled);

            let mut peer = wire::connect(server.addr()).expect("the holding peer connected");
            wire::write_request_with_connection(
                &mut peer,
                wire::KEEP_CONNECTION,
                "POST",
                "/held/1",
                &[],
                b"held-body",
            )
            .expect("the held request reached the server");
            entered
                .recv_timeout(SETTLE_BOUND)
                .expect("the handler reported entry while owning its admitted request");
            assert_eq!(
                cancelled.load(Ordering::Acquire),
                0,
                "the permit stays owned while the handler holds its request"
            );

            drop(peer);
            assert_released(&cancelled, 1, "request-future cancellation");
            assert_eq!(server.controller().observed().permit_owners_dropped, 1);

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── HTTP/2 refuses one stream and keeps the connection ───────────────

/// Assert one refused HTTP/2 stream: the answer, the one mapping it made, and
/// the identity that mapping was made under.
fn assert_h2_refused(
    answered: &wire::HttpResponse,
    mapped: &Journal,
    before: usize,
    target: &str,
    label: &str,
) {
    assert_eq!(answered.status, 413, "{label}: the stream was refused");
    assert_eq!(
        mapped_count(mapped) - before,
        1,
        "{label}: one refusal invokes the selected mapper once"
    );
    assert_last_mapped(mapped, RejectionKind::BodyLimit, PLAIN_ROUTE, target, label);
}

/// Assert every refusal recorded so far names a request of its own.
///
/// Three refusals carrying one identifier would be one request mapped three
/// times, which is the failure a per-stream claim exists to rule out.
fn assert_distinct_identities(mapped: &Journal, label: &str) {
    let journal = mapped.lock().unwrap_or_else(|error| error.into_inner());
    let recorded = journal.len();
    let mut identities: Vec<&str> = journal
        .iter()
        .map(|seen| seen.request_id.as_ref())
        .collect();
    identities.sort_unstable();
    identities.dedup();
    assert_eq!(
        identities.len(),
        recorded,
        "{label}: each refusal names its own request"
    );
}

/// Assert the declaration the h2 framing layer will not parse is refused below
/// Camber, and takes only its own stream with it.
///
/// Twenty digits is one more than h2 parses, so no request head is ever
/// constructed and there is no Camber identity, policy call, or mapped rejection
/// to claim for it — the HTTP/2 counterpart of
/// [`assert_undecodable_declaration_is_hypers`]. The peer ends this stream
/// rather than the connection, which is the disposition every other row here
/// asserts and the reason the reuse that follows still succeeds.
async fn assert_undecodable_h2_declaration_is_the_transports(
    client: &mut common::PersistentH2Client,
    calls: &Arc<AtomicUsize>,
    mapped: &Journal,
) {
    let mapped_before = mapped_count(mapped);
    let policy_before = calls.load(Ordering::SeqCst);
    let refused = client
        .try_send_withheld(
            "POST",
            "/plain/undecodable",
            "localhost",
            &[("content-length", UNDECODABLE_DECLARATION)],
        )
        .await;
    let error = refused.expect_err("a declaration h2 will not parse never becomes a request head");
    assert_eq!(
        error.reason(),
        Some(h2::Reason::PROTOCOL_ERROR),
        "the transport ended the stream itself: {error}"
    );
    assert_eq!(
        mapped_count(mapped),
        mapped_before,
        "a refusal below Camber claims no mapped rejection"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        policy_before,
        "a refusal below Camber reaches no body policy"
    );
}

/// Assert each refused HTTP/2 stream and the identity it was refused under.
async fn assert_h2_refusals(
    client: &mut common::PersistentH2Client,
    calls: &Arc<AtomicUsize>,
    mapped: &Journal,
) {
    let before = mapped_count(mapped);
    let declared = client
        .send_withheld(
            "POST",
            "/plain/1",
            "localhost",
            &[("content-length", "100000")],
        )
        .await;
    assert_h2_refused(
        &declared,
        mapped,
        before,
        "/plain/1",
        "declared oversize stream",
    );

    let before = mapped_count(mapped);
    let unholdable = client
        .send_withheld(
            "POST",
            "/plain/2",
            "localhost",
            &[("content-length", OVERSIZE_H2_DECLARATION)],
        )
        .await;
    assert_h2_refused(
        &unholdable,
        mapped,
        before,
        "/plain/2",
        "declaration wider than a 32-bit word",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the ceiling refused both before the policy"
    );

    let before = mapped_count(mapped);
    let under = [b'u'; CEILING - 4];
    let observed = client
        .send_paced("POST", "/plain/3", "localhost", &[], &[&under, b"crossing"])
        .await;
    assert_h2_refused(
        &observed,
        mapped,
        before,
        "/plain/3",
        "paced stream crossing its limit",
    );

    assert_undecodable_h2_declaration_is_the_transports(client, calls, mapped).await;
    assert_distinct_identities(mapped, "HTTP/2 refusals");
}

#[test]
fn http2_body_limit_refusals_are_stream_local_and_connection_remains_reusable() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let server = limited_server(&handled, &calls, &drops, &mapped);

            common::block_on(async {
                let mut client =
                    common::PersistentH2Client::connect(server.addr(), SETTLE_BOUND).await;
                assert_h2_refusals(&mut client, &calls, &mapped).await;

                let reused = client
                    .send_complete("POST", "/plain/4", "localhost", &[], b"still-serving")
                    .await;
                assert_eq!(
                    reused.status, 200,
                    "only the refused streams ended; the connection still serves"
                );
                assert_eq!(reused.body.as_ref(), b"still-serving");
                client.close().await;
            });

            assert_eq!(
                handled.load(Ordering::SeqCst),
                1,
                "only the admitted stream ran a handler"
            );
            assert_eq!(
                mapped_count(&mapped),
                3,
                "the admitted stream mapped no refusal"
            );
            assert_eq!(
                drops.load(Ordering::Acquire),
                0,
                "the plain route's policy hands over no permit"
            );
            assert_eq!(
                server.controller().observed().peak_retained_bytes,
                CEILING - 4,
                "no refused stream retained past its bound"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── A pre-read refusal leaves the transport as its protocol requires ──

/// The route whose policy declines the work outright.
const DECLINED_ROUTE: &str = "/declined/:id";
/// The route whose policy panics.
const PANICKING_ROUTE: &str = "/panicking/:id";
/// The prefix whose specialized gate refuses before any upstream.
const GATED_PREFIX: &str = "/gated";
/// The unique payload the panicking policy raises.
const POLICY_PANIC: &str = "policy-panic-payload";
/// The status the specialized gate answers a held upload with.
const GATE_STATUS: u16 = 403;
/// The declaration every pre-read row promises and never sends.
const WITHHELD_DECLARATION: &str = "8";

/// One request refused before a payload frame could be polled.
struct PreRead {
    label: &'static str,
    /// The route pattern the routing stage selected for this row.
    route: &'static str,
    path: &'static str,
    status: u16,
    /// The category the route's mapper recorded, when a mapper ran at all.
    ///
    /// A specialized gate answers with its own response rather than a refusal,
    /// so that row has no mapped category to name.
    kind: Option<RejectionKind>,
    /// Permit releases this row owes: one where admission granted a permit that
    /// a later stage then refused, none where admission itself refused.
    releases: usize,
}

/// Every pre-read row, in the order one fixture sends them.
fn pre_read_rows() -> [PreRead; 3] {
    [
        PreRead {
            label: "policy declined the work",
            route: DECLINED_ROUTE,
            path: "/declined/1",
            status: 503,
            kind: Some(RejectionKind::BodyAdmission),
            releases: 0,
        },
        PreRead {
            label: "policy panicked",
            route: PANICKING_ROUTE,
            path: "/panicking/1",
            status: 500,
            kind: Some(RejectionKind::InternalService),
            releases: 0,
        },
        PreRead {
            label: "specialized gate refused",
            route: "/gated/*proxy_path",
            path: "/gated/upstream",
            status: GATE_STATUS,
            kind: None,
            releases: 1,
        },
    ]
}

/// A policy that answers by route: decline, panic, or admit with a permit.
fn branching_policy(
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(calls);
    let drops = Arc::clone(drops);
    move |context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        match context.route() {
            DECLINED_ROUTE => Err(RuntimeError::InvalidArgument(
                "the policy declined this work".into(),
            )),
            PANICKING_ROUTE => panic!("{POLICY_PANIC}"),
            _ => Ok(BodyAdmission::with_permit(
                CEILING,
                DropProbe(Arc::clone(&drops)),
            )),
        }
    }
}

/// Serve the three pre-read refusals and one route that still answers.
fn pre_read_server(
    handled: &Arc<AtomicUsize>,
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
    mapped: &Journal,
) -> wire::ObservedServer<ScopedRequestBodyOwner> {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.post(DECLINED_ROUTE, counting_echo(handled));
    router.post(PANICKING_ROUTE, counting_echo(handled));
    router.post(PLAIN_ROUTE, counting_echo(handled));
    router.proxy_stream(GATED_PREFIX, "http://127.0.0.1:1");
    router.use_middleware(|request: &Request, next: camber::http::Next<'_>| {
        match request.path().starts_with(GATED_PREFIX) {
            false => next.call(request),
            true => Box::pin(async {
                Response::text(GATE_STATUS, "gate refused").expect("valid gate status")
            }) as Pin<Box<dyn Future<Output = Response> + Send>>,
        }
    });
    port.serve(
        router
            .max_request_body(CEILING)
            .body_admission(branching_policy(calls, drops))
            .rejection_mapper(recording_mapper(mapped, "router")),
    )
}

/// Assert one HTTP/1 row closes its connection behind the payload it withheld.
fn assert_http1_disposition(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    row: &PreRead,
    mapped: &Journal,
) {
    let released = server.controller().observed().permit_owners_dropped;
    let (refused, mut peer) = wire::send_withheld_declaration(
        server.addr(),
        wire::KEEP_CONNECTION,
        "POST",
        row.path,
        WITHHELD_DECLARATION,
    )
    .expect("the withheld pre-read row was answered");
    assert_eq!(refused.status, row.status, "{}: wire status", row.label);
    assert_eq!(
        refused.header("connection").map(str::to_ascii_lowercase),
        Some("close".to_owned()),
        "{}: HTTP/1 must not frame another request behind unread payload",
        row.label
    );
    assert_no_private_text(&refused, &[POLICY_PANIC], row.label);
    assert_row_owner(row, mapped);
    assert_eq!(
        server.controller().observed().frames_polled,
        0,
        "{}: nothing polls a frame before a pre-read refusal",
        row.label
    );
    assert_permit_delta(server, released, row);
    let reused = wire::probe_connection_reuse(&mut peer, "POST", "/plain/reuse", &[], b"probe");
    assert!(
        matches!(reused, Ok(None) | Err(_)),
        "{}: the connection must not frame another request: {reused:?}",
        row.label
    );
}

/// Assert which owner answered one pre-read row, and that it answered once.
///
/// A specialized gate answers with its own response rather than a refusal, so
/// that row's claim is that no mapper ran at all. Every other row names the
/// category, the route, and the request its own classification selected.
fn assert_row_owner(row: &PreRead, mapped: &Journal) {
    match row.kind {
        None => assert_eq!(mapped_count(mapped), 0, "{}: no mapper ran", row.label),
        Some(kind) => {
            assert_eq!(
                mapped_count(mapped),
                1,
                "{}: one refusal invokes one mapper once",
                row.label
            );
            assert_last_mapped(mapped, kind, row.route, row.path, row.label);
        }
    }
}

/// Assert one row released exactly the permits it was handed.
fn assert_permit_delta(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    released: usize,
    row: &PreRead,
) {
    let settled = wire::poll_until(SETTLE_BOUND, || {
        server.controller().observed().permit_owners_dropped == released + row.releases
    });
    assert!(
        settled,
        "{}: expected {} permit releases, saw {}",
        row.label,
        row.releases,
        server.controller().observed().permit_owners_dropped - released
    );
}

/// Assert one row ends its own HTTP/2 stream and nothing else.
///
/// Every claim the HTTP/1 helper makes except the one the transport answers
/// differently: an HTTP/2 refusal is stream-local, so the connection carries no
/// disposition at all. The rest — the mapped category, the request the row's own
/// classification named, an unmoved frame counter, and the permit release its
/// owner owed — are the same claims, made per row rather than in aggregate.
fn assert_http2_disposition(
    client: &mut common::PersistentH2Client,
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    row: &PreRead,
    mapped: &Journal,
) {
    let released = server.controller().observed().permit_owners_dropped;
    let polled = server.controller().observed().frames_polled;
    let refused = common::block_on(async {
        client
            .send_withheld(
                "POST",
                row.path,
                "localhost",
                &[("content-length", WITHHELD_DECLARATION)],
            )
            .await
    });
    assert_eq!(refused.status, row.status, "{}: HTTP/2 status", row.label);
    assert_eq!(
        refused.header("connection"),
        None,
        "{}: HTTP/2 carries no connection disposition",
        row.label
    );
    assert_no_private_text(&refused, &[POLICY_PANIC], row.label);
    assert_row_owner(row, mapped);
    assert_eq!(
        server.controller().observed().frames_polled,
        polled,
        "{}: nothing polls a frame before a pre-read refusal",
        row.label
    );
    assert_permit_delta(server, released, row);
}

#[test]
fn pre_read_policy_and_gate_refusals_have_protocol_correct_transport_disposition() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let server = pre_read_server(&handled, &calls, &drops, &mapped);

            for row in &pre_read_rows() {
                assert_http1_disposition(&server, row, &mapped);
                drain(&mapped);
            }
            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "every row reached its policy exactly once"
            );
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "no pre-read refusal runs a handler"
            );

            // One `block_on` per exchange rather than one around the whole
            // sequence: the counters each row is measured against are read
            // synchronously, and a bounded wait for a permit release inside the
            // driver would be a blocking sleep on the thread driving it.
            let mut client = common::block_on(async {
                common::PersistentH2Client::connect(server.addr(), SETTLE_BOUND).await
            });
            for row in &pre_read_rows() {
                assert_http2_disposition(&mut client, &server, row, &mapped);
                drain(&mapped);
            }
            let reused = common::block_on(async {
                client
                    .send_complete("POST", "/plain/9", "localhost", &[], b"still-serving")
                    .await
            });
            assert_eq!(
                reused.status, 200,
                "only the refused streams ended; the connection still serves"
            );
            assert_eq!(reused.body.as_ref(), b"still-serving");
            common::block_on(async { client.close().await });

            assert_eq!(
                handled.load(Ordering::SeqCst),
                1,
                "only the admitted stream ran a handler"
            );
            // Two gate refusals and one admitted stream: every row that
            // reached a permit released it, and no refused row took one.
            assert_released(&drops, 3, "pre-read rows");

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Every class one full-feature router answers, on one listener ─────

/// Fixtures for the cross-class claim: every route class a full-feature router
/// can answer, served together so the counters one row moves are the counters
/// every other row is measured against.
#[cfg(all(feature = "ws", feature = "grpc"))]
mod cross_class {
    use super::{DropProbe, counting_echo};
    use crate::common;
    use crate::http as wire;

    use camber::http::{
        BodyAdmission, BodyAdmissionContext, GrpcRouter, RejectionKind, Request, RequestBodyMode,
        Response, Router, SseWriter, StreamResponse, WsConn, mock::ScopedRequestBodyOwner,
    };
    use camber::{Resource, RuntimeError};
    use common::{Journal, drain, recording_mapper};
    use std::future::Future;
    use std::io::Write;
    use std::net::SocketAddr;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The ceiling every consuming row here is admitted under.
    pub(super) const CROSS_CEILING: usize = 4096;
    /// The route whose ordinary handler echoes what it was given.
    const ECHO_ROUTE: &str = "/echo/:id";
    /// The route whose handler answers with a produced stream.
    const STREAM_ROUTE: &str = "/streamed/:id";
    /// The prefix a buffered proxy claims.
    const BUFFERED_PREFIX: &str = "/buffered";
    /// The prefix a streaming proxy claims.
    const STREAMING_PREFIX: &str = "/streaming";
    /// The prefix a buffered proxy claims for upgrade traffic.
    const WS_BUFFERED_PREFIX: &str = "/ws-buffered";
    /// The prefix a streaming proxy claims for upgrade traffic.
    const WS_STREAMING_PREFIX: &str = "/ws-streaming";
    /// The route that answers server-sent events.
    const SSE_ROUTE: &str = "/events";
    /// The route that answers a direct WebSocket upgrade.
    const WS_ROUTE: &str = "/socket";
    /// The route registered for one method only.
    const ONLY_GET: &str = "/only-get";
    /// The prefix whose buffered upstream is never well.
    const DOWN_PREFIX: &str = "/down";
    /// The prefix whose streaming upstream is never well.
    const DOWN_STREAM_PREFIX: &str = "/down-stream";
    /// The message every WebSocket peer here is greeted with.
    const WS_GREETING: &str = "socket-open";
    /// The event every SSE row is answered with.
    const SSE_EVENT: &str = "one-event";
    /// The body every consuming row sends.
    pub(super) const CONSUMED: &[u8] = b"consumed-payload";
    /// The declaration every terminal row promises and never sends.
    const WITHHELD: &str = "8";
    /// The host every row addresses.
    const HOST: &str = "localhost";
    /// The one mapper this listener's refusals reach.
    const CROSS_MAPPER: &str = "cross-class";
    /// How long one live exchange has to complete.
    const CROSS_BOUND: Duration = Duration::from_secs(5);

    mod proto {
        tonic::include_proto!("greeter");
    }

    use proto::greeter_service;

    struct Greeter;

    #[tonic::async_trait]
    impl greeter_service::Greeter for Greeter {
        async fn say_hello(
            &self,
            request: tonic::Request<proto::HelloRequest>,
        ) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
            let name = request.into_inner().name;
            Ok(tonic::Response::new(proto::HelloReply {
                message: format!("Hello, {name}!"),
            }))
        }
    }

    /// A resource that is always well, so `/health` exists to be routed to.
    pub(super) struct AlwaysHealthy;

    impl Resource for AlwaysHealthy {
        fn name(&self) -> &str {
            "cross-class"
        }

        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn shutdown(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    /// What one admission callback was given, and what had been polled by then.
    pub(super) struct Admitted {
        pub(super) route: Box<str>,
        pub(super) streaming: bool,
        pub(super) polled: usize,
    }

    /// Every policy call one fixture observed, in order.
    pub(super) type Admissions = Arc<Mutex<Vec<Admitted>>>;

    /// Every counter one cross-class fixture reads.
    pub(super) struct Observations {
        pub(super) admitted: Admissions,
        pub(super) handled: Arc<AtomicUsize>,
        pub(super) drops: Arc<AtomicUsize>,
        /// Every refusal this listener's one mapper was handed.
        ///
        /// A terminal row's status alone does not say who answered it: the same
        /// `404` is what a routing refusal and a silently swallowed one both
        /// look like on the wire. The mapper is what names the category and the
        /// request each row's own classification selected.
        pub(super) mapped: Journal,
    }

    impl Observations {
        pub(super) fn new() -> Self {
            Self {
                admitted: Arc::new(Mutex::new(Vec::new())),
                handled: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
                mapped: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub(super) fn admissions(&self) -> usize {
            self.admitted
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .len()
        }

        pub(super) fn handled(&self) -> usize {
            self.handled.load(Ordering::SeqCst)
        }

        pub(super) fn drops(&self) -> usize {
            self.drops.load(Ordering::Acquire)
        }
    }

    /// A policy that journals every request it is asked about, then admits it.
    fn journalling_policy(
        observations: &Observations,
        controller: &Arc<ScopedRequestBodyOwner>,
    ) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
    {
        let admitted = Arc::clone(&observations.admitted);
        let drops = Arc::clone(&observations.drops);
        let controller = Arc::clone(controller);
        move |context: &BodyAdmissionContext<'_>| {
            admitted
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(Admitted {
                    route: context.route().into(),
                    streaming: context.mode() == RequestBodyMode::Streaming,
                    polled: controller.observed().frames_polled,
                });
            Ok(BodyAdmission::with_permit(
                CROSS_CEILING,
                DropProbe(Arc::clone(&drops)),
            ))
        }
    }

    /// Serve the upstream both proxy prefixes forward ordinary traffic to.
    fn ordinary_upstream() -> SocketAddr {
        let mut upstream = Router::new();
        upstream.post("/echo", |request: &Request| {
            let body: Box<str> = request.body().into();
            async move { Response::text(200, &body).expect("valid upstream status") }
        });
        common::spawn_server(upstream)
    }

    /// Serve the upstream both proxy prefixes forward upgrade traffic to.
    fn upgrade_upstream() -> SocketAddr {
        let mut upstream = Router::new();
        upstream.ws("/echo", |_request: &Request, conn: WsConn| {
            conn.send(WS_GREETING)?;
            Ok(())
        });
        common::spawn_server(upstream)
    }

    /// Register every class one full-feature router can answer.
    fn full_feature_routes(
        router: &mut Router,
        observations: &Observations,
        ordinary: SocketAddr,
        upgrades: SocketAddr,
    ) {
        let handled = Arc::clone(&observations.handled);
        router.post(ECHO_ROUTE, counting_echo(&handled));
        router.post_stream(STREAM_ROUTE, |request: &Request| {
            let body: Box<str> = request.body().into();
            Box::pin(async move {
                let (response, sender) = StreamResponse::new(200);
                sender
                    .send(body.as_bytes().to_vec())
                    .await
                    .expect("the stream was open");
                response
            }) as Pin<Box<dyn Future<Output = StreamResponse> + Send>>
        });
        router.get_sse(SSE_ROUTE, |_request: &Request, writer: &mut SseWriter| {
            writer.event("message", SSE_EVENT)
        });
        router.ws(WS_ROUTE, |_request: &Request, conn: WsConn| {
            conn.send(WS_GREETING)?;
            Ok(())
        });
        router.get(ONLY_GET, |_request: &Request| async {
            Response::text(200, "only-get").expect("valid status")
        });
        router.grpc(GrpcRouter::new().add_service(greeter_service::serve(Greeter)));
        router.proxy(BUFFERED_PREFIX, &format!("http://{ordinary}"));
        router.proxy_stream(STREAMING_PREFIX, &format!("http://{ordinary}"));
        router.proxy(WS_BUFFERED_PREFIX, &format!("http://{upgrades}"));
        router.proxy_stream(WS_STREAMING_PREFIX, &format!("http://{upgrades}"));
        let unwell = Arc::new(AtomicBool::new(false));
        router.proxy_checked(DOWN_PREFIX, "http://127.0.0.1:1", Arc::clone(&unwell));
        router.proxy_checked_stream(DOWN_STREAM_PREFIX, "http://127.0.0.1:1", unwell);
    }

    /// Serve every class on one observed listener.
    ///
    /// A single router rather than a host table, because gRPC dispatch is a
    /// single-router class in this codebase: a host table answers no gRPC
    /// handoff at all, and the handoff is one of the bodyless classes this case
    /// exists to measure. The authority row below is written to that same fact.
    pub(super) fn full_feature_server(
        observations: &Observations,
    ) -> wire::ObservedServer<ScopedRequestBodyOwner> {
        let port = wire::reserve_request_body_owner();
        let controller = port.controller();
        let mut router = Router::new();
        full_feature_routes(
            &mut router,
            observations,
            ordinary_upstream(),
            upgrade_upstream(),
        );
        port.serve(
            router
                .max_request_body(CROSS_CEILING)
                .body_admission(journalling_policy(observations, &controller))
                .rejection_mapper(recording_mapper(&observations.mapped, CROSS_MAPPER)),
        )
    }

    /// Assert every bodyless class answers exactly as its own protocol says.
    pub(super) fn assert_bodyless_rows(addr: SocketAddr) {
        let health = wire::send_to_host(addr, "GET", "/health", HOST);
        assert_eq!(health.status, 200, "the internal health route answered");

        assert_sse(addr);
        assert_handshake(addr, "/socket", "direct upgrade");
        assert_handshake(addr, "/ws-buffered/echo", "buffered-proxy upgrade");
        assert_handshake(addr, "/ws-streaming/echo", "streaming-proxy upgrade");
        assert_grpc(addr);
    }

    /// Assert the SSE route answers its own stream, event and all.
    ///
    /// Read to the end of the body rather than to the end of the head: the head
    /// says a stream was opened, and the event is the only thing that says the
    /// producer behind it ran and reached the peer.
    fn assert_sse(addr: SocketAddr) {
        let answered = wire::send_to_host(addr, "GET", SSE_ROUTE, HOST);
        assert_eq!(answered.status, 200, "SSE status");
        assert_eq!(
            answered.header("content-type"),
            Some("text/event-stream"),
            "SSE representation"
        );
        assert_eq!(
            answered.text().as_ref(),
            format!("event: message\ndata: {SSE_EVENT}\n\n"),
            "the emitted event reached the peer"
        );
    }

    /// Assert one upgrade path completes its handshake and greets the peer.
    fn assert_handshake(addr: SocketAddr, path: &str, label: &str) {
        let mut peer = wire::connect(addr).expect("the upgrading peer connected");
        peer.write_all(common::ws_upgrade_request_to(HOST, path).as_bytes())
            .and_then(|()| peer.flush())
            .expect("the upgrade request reached the server");
        let answered = wire::read_head(&mut peer, CROSS_BOUND).expect("the handshake was answered");
        let text = String::from_utf8_lossy(&answered);
        assert!(
            text.starts_with("HTTP/1.1 101"),
            "{label}: handshake: {text}"
        );
        assert_eq!(
            common::read_ws_text_frame(&mut peer).as_ref(),
            WS_GREETING,
            "{label}: the upgraded peer was greeted"
        );
        common::write_ws_close_frame(&mut peer);
    }

    /// Assert the gRPC handoff answers through its generated client.
    fn assert_grpc(addr: SocketAddr) {
        let reply = common::block_on(async {
            let channel = tokio::time::timeout(
                CROSS_BOUND,
                tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .expect("a valid gRPC endpoint")
                    .connect(),
            )
            .await
            .expect("the gRPC channel connected in time")
            .expect("the gRPC channel connected");
            let mut client = proto::greeter_client::GreeterClient::new(channel);
            tokio::time::timeout(
                CROSS_BOUND,
                client.say_hello(proto::HelloRequest {
                    name: "Cross".into(),
                }),
            )
            .await
            .expect("the unary call answered in time")
            .expect("the unary call succeeded")
            .into_inner()
        });
        assert_eq!(reply.message, "Hello, Cross!");
    }

    /// One request the routing stage answers before any body is admitted.
    struct Terminal {
        label: &'static str,
        method: &'static str,
        path: Box<str>,
        host: &'static str,
        status: u16,
        /// The category this row's owner classified it as.
        kind: RejectionKind,
        /// The route pattern the routing stage selected, when it selected one.
        route: Option<&'static str>,
    }

    /// Every routing terminal, in the order one fixture sends them.
    fn terminal_rows() -> [Terminal; 7] {
        [
            Terminal {
                label: "no route claims the path",
                method: "POST",
                path: "/missing".into(),
                host: HOST,
                status: 404,
                kind: RejectionKind::Routing,
                route: None,
            },
            Terminal {
                label: "ordinary method mismatch",
                method: "POST",
                path: ONLY_GET.into(),
                host: HOST,
                status: 405,
                kind: RejectionKind::MethodSelection,
                route: Some(ONLY_GET),
            },
            Terminal {
                label: "unnameable method",
                method: "BREW",
                path: ONLY_GET.into(),
                host: HOST,
                status: 405,
                kind: RejectionKind::MethodSelection,
                route: Some(ONLY_GET),
            },
            // A single router resolves no authority, so an unparseable one
            // reaches the trie like any other head. What is claimed here is
            // what still holds for it: the terminal it lands on is answered
            // from the head, with no body policy and no payload read.
            Terminal {
                label: "authority that is not one",
                method: "POST",
                path: "/missing".into(),
                host: "not an authority",
                status: 404,
                kind: RejectionKind::Routing,
                route: None,
            },
            Terminal {
                label: "URI too deep to route",
                method: "POST",
                path: "/deep".repeat(80).into(),
                host: HOST,
                status: 414,
                kind: RejectionKind::Routing,
                route: None,
            },
            Terminal {
                label: "buffered proxy with an unhealthy upstream",
                method: "POST",
                path: "/down/echo".into(),
                host: HOST,
                status: 503,
                kind: RejectionKind::Proxy,
                route: Some("/down/*proxy_path"),
            },
            Terminal {
                label: "streaming proxy with an unhealthy upstream",
                method: "POST",
                path: "/down-stream/echo".into(),
                host: HOST,
                status: 503,
                kind: RejectionKind::Proxy,
                route: Some("/down-stream/*proxy_path"),
            },
        ]
    }

    /// Assert every routing terminal answers from the head alone.
    ///
    /// The wire status is half the row. The other half is who answered it: the
    /// listener's own mapper, once, with the category its producer classified
    /// and the request identity that same classification selected. Without it a
    /// row proves only that some stage produced a number.
    pub(super) fn assert_terminal_rows(addr: SocketAddr, observations: &Observations) {
        for row in &terminal_rows() {
            let answered = wire::send_to_host_with(
                addr,
                row.method,
                &row.path,
                row.host,
                &[("Content-Length", WITHHELD)],
            );
            assert_eq!(answered.status, row.status, "{}: wire status", row.label);
            assert_terminal_owner(row, &observations.mapped);
        }
    }

    /// Assert one terminal row reached this listener's mapper exactly once.
    fn assert_terminal_owner(row: &Terminal, mapped: &Journal) {
        let seen = drain(mapped);
        assert_eq!(
            seen.len(),
            1,
            "{}: one terminal invokes one mapper once: {seen:?}",
            row.label
        );
        let seen = &seen[0];
        assert_eq!(seen.origin, CROSS_MAPPER, "{}: mapper", row.label);
        assert_eq!(seen.kind, row.kind, "{}: category", row.label);
        assert_eq!(seen.status, row.status, "{}: classified status", row.label);
        assert_eq!(seen.method.as_ref(), row.method, "{}: method", row.label);
        assert_eq!(
            seen.raw_path.as_ref(),
            row.path.as_ref(),
            "{}: raw path",
            row.label
        );
        assert_eq!(seen.route.as_deref(), row.route, "{}: route", row.label);
        common::assert_request_id_shape(Some(seen.request_id.as_ref()), row.label);
    }

    /// Assert every consuming class answers exactly, admitted before its first poll.
    pub(super) fn assert_consuming_rows(
        server: &wire::ObservedServer<ScopedRequestBodyOwner>,
        observations: &Observations,
    ) {
        let rows: [(&str, &str, bool); 4] = [
            (ECHO_ROUTE, "/echo/1", false),
            (STREAM_ROUTE, "/streamed/1", false),
            ("/buffered/*proxy_path", "/buffered/echo", false),
            ("/streaming/*proxy_path", "/streaming/echo", true),
        ];
        for (route, path, streaming) in rows {
            let polled = server.controller().observed().frames_polled;
            let before = observations.admissions();
            let answered =
                wire::request_to_host_with_body(server.addr(), "POST", path, HOST, &[], CONSUMED)
                    .expect("the consuming row answered");
            assert_eq!(answered.status, 200, "{path}: wire status");
            assert_eq!(
                answered.text().as_ref(),
                String::from_utf8_lossy(CONSUMED).as_ref(),
                "{path}: the payload reached its consumer intact"
            );
            assert_admitted_once(observations, before, route, streaming, polled, path);
        }
    }

    /// Assert one consuming row was admitted once, before its first payload poll.
    fn assert_admitted_once(
        observations: &Observations,
        before: usize,
        route: &str,
        streaming: bool,
        polled: usize,
        label: &str,
    ) {
        let journal = observations
            .admitted
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(journal.len(), before + 1, "{label}: one admission");
        let entry = journal.last().expect("the policy journalled its context");
        assert_eq!(entry.route.as_ref(), route, "{label}: route");
        assert_eq!(entry.streaming, streaming, "{label}: mode");
        assert_eq!(
            entry.polled, polled,
            "{label}: admission precedes this request's first payload poll"
        );
    }
}

#[cfg(all(feature = "ws", feature = "grpc"))]
#[test]
fn all_bodyless_and_pre_admission_terminal_classes_skip_policy_and_frames() {
    common::test_runtime()
        .resource(cross_class::AlwaysHealthy)
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let observations = cross_class::Observations::new();
            let server = cross_class::full_feature_server(&observations);

            cross_class::assert_bodyless_rows(server.addr());
            cross_class::assert_terminal_rows(server.addr(), &observations);

            assert_eq!(
                observations.admissions(),
                0,
                "no bodyless or terminal class invokes a body policy"
            );
            assert_eq!(
                observations.drops(),
                0,
                "no bodyless or terminal class acquires a permit"
            );
            assert_eq!(
                observations.handled(),
                0,
                "no bodyless or terminal class runs an ordinary handler"
            );
            assert_eq!(
                server.controller().observed().frames_polled,
                0,
                "no bodyless or terminal class polls a payload frame"
            );
            assert_eq!(
                server.controller().observed().peak_retained_bytes,
                0,
                "no bodyless or terminal class retains payload bytes"
            );

            cross_class::assert_consuming_rows(&server, &observations);

            assert_eq!(
                observations.admissions(),
                4,
                "every consuming class was admitted exactly once"
            );
            assert!(
                server.controller().observed().frames_polled > 0,
                "the same counters move once a body-consuming class reads a payload"
            );
            assert_eq!(
                server.controller().observed().peak_retained_bytes,
                cross_class::CONSUMED.len(),
                "a buffered consumer retains exactly what it was sent"
            );
            assert_released(&observations.drops, 4, "consuming classes");

            runtime::request_shutdown();
        })
        .unwrap();
}
