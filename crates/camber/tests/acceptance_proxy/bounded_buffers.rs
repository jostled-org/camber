//! 8.T2: a buffered proxy route collects its upstream answer under the one
//! ceiling that route froze.

use crate::common;

use camber::http::mock::{self, LifecycleController};
use camber::http::{ProxyPolicy, RejectionKind, Router};
use camber::runtime;
use std::net::SocketAddr;
use std::time::Duration;

/// The buffered ceiling every row but the opted-out one is measured against.
const CEILING: usize = 16;

/// The payload that exactly fills [`CEILING`].
const ADMITTED: &str = "0123456789abcdef";

/// The payload one crossing chunk carries past [`CEILING`].
///
/// Distinct text, not more of the admitted bytes: a row proving the crossing
/// frame reached no peer has to be able to name it in what the peer received.
const CROSSING: &str = "crossing";

/// Everything a crossing upstream answer carries: the admitted bytes and the
/// frame past them.
const CROSSED: usize = ADMITTED.len() + CROSSING.len();

/// The prefix every row's proxy route is mounted under.
const PREFIX: &str = "/api";

/// How long one row waits for the answer its proxy already settled.
const ROW_BOUND: Duration = Duration::from_secs(5);

/// The redacted body a bad gateway is answered with.
///
/// Read from the peer, so this is the production sentence and not a paraphrase:
/// a row asserting redaction is asserting the peer got exactly this and no
/// upstream text at all.
const BAD_GATEWAY_BODY: &str = "bad gateway";

/// The typed cause an operator reads when a buffered proxy ceiling is crossed.
const PROXY_CEILING_CAUSE: &str = "cause=byte limit exceeded: proxy_buffered_response";

/// A chunked answer head, which declares no length at all.
const CHUNKED_HEAD: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

/// One scripted upstream answer, assembled as the bytes a row puts on the wire.
fn answer_bytes(head: &str, body: &str) -> Box<[u8]> {
    format!("{head}{body}").into_bytes().into_boxed_slice()
}

/// The head of an answer stating `declared` bytes, with `framing` after it.
fn declared_head(declared: usize, framing: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n{framing}\r\n")
}

/// An answer that declares `declared` bytes and sends `body`.
fn declared_answer(declared: usize, body: &str) -> Box<[u8]> {
    answer_bytes(&declared_head(declared, ""), body)
}

/// A declared answer whose connection this upstream ends after answering it.
///
/// The per-route case sends two requests to one backend, and Camber's upstream
/// client pools connections while this fixture answers each connection exactly
/// once. Closing is what keeps the second route's request on a connection this
/// upstream will still answer, so the case measures two frozen maxima rather
/// than a pool.
fn closing_answer(declared: usize, body: &str) -> Box<[u8]> {
    answer_bytes(&declared_head(declared, "Connection: close\r\n"), body)
}

/// A chunked answer carrying each payload as its own frame, terminated.
fn chunked_answer(frames: &[&str]) -> Box<[u8]> {
    answer_bytes(
        CHUNKED_HEAD,
        &frames
            .iter()
            .map(|frame| format!("{:x}\r\n{frame}\r\n", frame.len()))
            .chain(std::iter::once(String::from("0\r\n\r\n")))
            .collect::<String>(),
    )
}

/// The bytes a crossing upstream answer carries, admitted frame first.
fn crossed_payload() -> Box<str> {
    format!("{ADMITTED}{CROSSING}").into()
}

/// What one row expects its peer to have been answered with.
enum Expected {
    /// The upstream payload arrived whole under a 200.
    Forwarded(Box<str>),
    /// The route refused the answer as a bad gateway.
    Refused,
}

/// What the production collector must have published once a row settled.
///
/// Named per row rather than derived from the answer the peer got: a refusal on
/// a trustworthy declaration reads no frame at all, a refusal on a crossing
/// frame reads one and keeps none of it, and both peers see the same 502.
struct Retention {
    /// Whether any frame reached the collector before it answered.
    polled: bool,
    /// The most this row's collection ever held at once.
    peak: usize,
}

/// One buffered-collection row: an upstream script, a route policy, the answer
/// the peer is owed, and what the collector may have kept producing it.
struct Row {
    label: &'static str,
    path: &'static str,
    answer: Box<[u8]>,
    policy: ProxyPolicy,
    expected: Expected,
    retention: Retention,
}

/// The finite ceiling one route freezes.
fn frozen(limit: usize) -> ProxyPolicy {
    ProxyPolicy::default()
        .buffered_response_limit(limit)
        .expect("a finite buffered ceiling")
}

/// Every row this proof runs, in the order it runs them.
fn rows() -> Box<[Row]> {
    Box::new([
        Row {
            label: "exact limit",
            path: "/exact",
            answer: declared_answer(ADMITTED.len(), ADMITTED),
            policy: frozen(CEILING),
            expected: Expected::Forwarded(ADMITTED.into()),
            retention: Retention {
                polled: true,
                peak: ADMITTED.len(),
            },
        },
        Row {
            label: "declared oversize",
            path: "/declared-oversize",
            answer: declared_answer(CROSSED, &crossed_payload()),
            policy: frozen(CEILING),
            expected: Expected::Refused,
            retention: Retention {
                polled: false,
                peak: 0,
            },
        },
        Row {
            label: "chunked crossing",
            path: "/chunked-crossing",
            answer: chunked_answer(&[ADMITTED, CROSSING]),
            policy: frozen(CEILING),
            expected: Expected::Refused,
            retention: Retention {
                polled: true,
                peak: CEILING,
            },
        },
        Row {
            label: "explicit unbounded",
            path: "/unbounded",
            answer: chunked_answer(&[ADMITTED, CROSSING]),
            policy: frozen(CEILING).unbounded_buffered_response(),
            expected: Expected::Forwarded(crossed_payload()),
            retention: Retention {
                polled: true,
                peak: CROSSED,
            },
        },
    ])
}

/// Watch the upstream a case reads from, so the case reads what the production
/// collector published while it collected that upstream's answer.
fn watch(addr: SocketAddr) -> LifecycleController {
    mock::lifecycle(addr).expect("one controller for this upstream")
}

/// Run one row end to end against a real upstream and a real peer.
fn run_row(row: Row) {
    let upstream = common::scripted_upstream(row.answer, common::UpstreamAnswers::OnHead);
    let observed = watch(upstream.addr());
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
    router.proxy_with_policy(PREFIX, &upstream.backend(), row.policy);
    let served = common::spawn_server(router);

    let target: Box<str> = format!("{PREFIX}{}", row.path).into();
    let captured = common::capture_events(&format!("raw_path={target}"));
    let response = common::request(served, "GET", &target, &[], b"", ROW_BOUND)
        .unwrap_or_else(|error| panic!("{}: the proxy never answered: {error}", row.label));

    match row.expected {
        Expected::Forwarded(payload) => {
            assert_forwarded(row.label, &response, &payload, &journal);
        }
        Expected::Refused => assert_refused(row.label, &response, &journal, &captured, PREFIX),
    }
    assert_retention(row.label, &observed, &row.retention);
}

/// An admitted answer reaches the peer whole and invokes no mapper.
fn assert_forwarded(
    row: &str,
    response: &common::HttpResponse,
    payload: &str,
    journal: &common::Journal,
) {
    assert_eq!(response.status, 200, "{row}: unexpected status");
    assert_eq!(
        response.body.as_ref(),
        payload.as_bytes(),
        "{row}: the admitted payload must arrive whole",
    );
    assert!(
        common::drain(journal).is_empty(),
        "{row}: an admitted answer refuses nothing",
    );
}

/// A crossed ceiling maps once to a redacted bad gateway, and the upstream
/// payload reaches no peer.
fn assert_refused(
    row: &str,
    response: &common::HttpResponse,
    journal: &common::Journal,
    captured: &common::TraceCapture,
    prefix: &str,
) {
    assert_eq!(response.status, 502, "{row}: unexpected status");
    assert_eq!(
        response.body.as_ref(),
        BAD_GATEWAY_BODY.as_bytes(),
        "{row}: the peer is told only that the gateway failed",
    );
    common::assert_no_private_text(response, &[ADMITTED, CROSSING], row);

    let seen = common::only(journal, row);
    assert_eq!(
        seen.kind,
        RejectionKind::Proxy,
        "{row}: the refusal keeps its proxy category",
    );
    assert_eq!(seen.status, 502, "{row}: the mapped status");
    assert_eq!(
        seen.route.as_deref(),
        Some(format!("{prefix}/*proxy_path").as_str()),
        "{row}: the refusal names the route that froze the ceiling",
    );

    // The peer learned nothing; the operator learned which bound was crossed.
    let events = captured.events();
    let recorded = common::only_event(&events, common::REJECTION_MESSAGE, row);
    common::assert_fields(recorded, &[PROXY_CEILING_CAUSE, "kind=proxy"], row);
}

/// What the production collector read from this upstream, and the most it held.
///
/// Both numbers are published by the collector itself after every accounting
/// decision, refusals included, so this reads the buffer rather than the total
/// the collection was permitted: a collector that appended the crossing frame
/// and only then refused it would report those bytes here, and a peer-visible
/// 502 would hide them.
fn assert_retention(row: &str, observed: &LifecycleController, expected: &Retention) {
    assert_eq!(
        observed.collected_chunks_polled() > 0,
        expected.polled,
        "{row}: a maximum a declaration already crossed is refused before a \
         frame is read, and one only a frame crosses is not",
    );
    assert_eq!(
        observed.collected_peak_retained_bytes(),
        expected.peak,
        "{row}: no collection may ever hold more than its own maximum admits",
    );
}

/// The prefix of the route whose frozen maximum refuses the shared answer.
const TIGHT_PREFIX: &str = "/tight";

/// The prefix of the route whose frozen maximum admits it.
const WIDE_PREFIX: &str = "/wide";

/// Two routes on one router, one backend, one answer, two frozen maxima.
///
/// This is what makes the ceiling the *route's*. Every row above mounts a
/// single route, so none of them can tell a maximum that route froze from one
/// the router, the served process, or a shared default holds: one route under
/// one policy answers the same way under all four. Here the same upstream
/// answers both routes with the same bytes through one server, and the two
/// answers differ — the tight route refuses what the wide route forwards whole,
/// and the collector read nothing at all for the first and everything for the
/// second.
fn per_route_maxima_each_apply() {
    let upstream = common::scripted_upstream(
        closing_answer(CROSSED, &crossed_payload()),
        common::UpstreamAnswers::OnHead,
    );
    let observed = watch(upstream.addr());
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
    router.proxy_with_policy(TIGHT_PREFIX, &upstream.backend(), frozen(CEILING));
    router.proxy_with_policy(WIDE_PREFIX, &upstream.backend(), frozen(CROSSED));
    let served = common::spawn_server(router);

    let tight = "tight route";
    let refused_target = format!("{TIGHT_PREFIX}/answer");
    let captured = common::capture_events(&format!("raw_path={refused_target}"));
    let refused = common::request(served, "GET", &refused_target, &[], b"", ROW_BOUND)
        .unwrap_or_else(|error| panic!("{tight}: the proxy never answered: {error}"));
    assert_refused(tight, &refused, &journal, &captured, TIGHT_PREFIX);
    assert_retention(
        tight,
        &observed,
        &Retention {
            polled: false,
            peak: 0,
        },
    );

    let wide = "wide route";
    let forwarded_target = format!("{WIDE_PREFIX}/answer");
    let answered = common::request(served, "GET", &forwarded_target, &[], b"", ROW_BOUND)
        .unwrap_or_else(|error| panic!("{wide}: the proxy never answered: {error}"));
    assert_forwarded(wide, &answered, &crossed_payload(), &journal);
    assert_retention(
        wide,
        &observed,
        &Retention {
            polled: true,
            peak: CROSSED,
        },
    );
}

/// 8.T2
#[test]
fn buffered_proxy_applies_selected_ceiling_without_crossing_retention() {
    common::test_runtime()
        .with_tracing()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            for row in rows() {
                run_row(row);
            }
            per_route_maxima_each_apply();
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
