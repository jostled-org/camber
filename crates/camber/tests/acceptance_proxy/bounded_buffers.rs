//! 8.T2: a buffered proxy route collects its upstream answer under the one
//! ceiling that route froze.

use crate::common;

use camber::http::{ProxyPolicy, RejectionKind, Router};
use camber::runtime;
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

/// An answer that declares `declared` bytes and sends `body`.
fn declared_answer(declared: usize, body: &str) -> Box<[u8]> {
    answer_bytes(
        &format!("HTTP/1.1 200 OK\r\nContent-Length: {declared}\r\n\r\n"),
        body,
    )
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

/// What one row expects its peer to have been answered with.
enum Expected {
    /// The upstream payload arrived whole under a 200.
    Forwarded(&'static str),
    /// The route refused the answer as a bad gateway.
    Refused,
}

/// One buffered-collection row: an upstream script, a route policy, and the
/// answer the peer is owed.
struct Row {
    label: &'static str,
    path: &'static str,
    answer: Box<[u8]>,
    policy: ProxyPolicy,
    expected: Expected,
}

/// The finite ceiling every bounded row's route freezes.
fn bounded_policy() -> ProxyPolicy {
    ProxyPolicy::default()
        .buffered_response_limit(CEILING)
        .expect("a finite buffered ceiling")
}

/// Every row this proof runs, in the order it runs them.
fn rows() -> Box<[Row]> {
    Box::new([
        Row {
            label: "exact limit",
            path: "/exact",
            answer: declared_answer(ADMITTED.len(), ADMITTED),
            policy: bounded_policy(),
            expected: Expected::Forwarded(ADMITTED),
        },
        Row {
            label: "declared oversize",
            path: "/declared-oversize",
            answer: declared_answer(
                ADMITTED.len() + CROSSING.len(),
                &format!("{ADMITTED}{CROSSING}"),
            ),
            policy: bounded_policy(),
            expected: Expected::Refused,
        },
        Row {
            label: "chunked crossing",
            path: "/chunked-crossing",
            answer: chunked_answer(&[ADMITTED, CROSSING]),
            policy: bounded_policy(),
            expected: Expected::Refused,
        },
        Row {
            label: "explicit unbounded",
            path: "/unbounded",
            answer: chunked_answer(&[ADMITTED, CROSSING]),
            policy: bounded_policy().unbounded_buffered_response(),
            expected: Expected::Forwarded("0123456789abcdefcrossing"),
        },
    ])
}

/// Run one row end to end against a real upstream and a real peer.
fn run_row(row: Row) {
    let upstream = common::scripted_upstream(row.answer, common::UpstreamAnswers::OnHead);
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
            assert_forwarded(row.label, &response, payload, &journal);
        }
        Expected::Refused => assert_refused(row.label, &response, &journal, &captured),
    }
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
        Some("/api/*proxy_path"),
        "{row}: the refusal names the route that froze the ceiling",
    );

    // The peer learned nothing; the operator learned which bound was crossed.
    let events = captured.events();
    let recorded = common::only_event(&events, common::REJECTION_MESSAGE, row);
    common::assert_fields(recorded, &[PROXY_CEILING_CAUSE, "kind=proxy"], row);
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
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
