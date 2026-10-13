//! Outbound 7.T1 and 7.T2: what one outbound operation records, and what it
//! sends as trace context.
//!
//! 7.T1 reads the `http.client.completed` event production emits. One started
//! operation records one event, on return or when its pending future is
//! dropped. The event carries these fields and no part of the request:
//!
//! - `method`: a standard method label, or `OTHER` for an extension method.
//! - `host`: the host of the URL the call began at, without its port.
//! - `status`: the status of the final response head, or `none`. A head the
//!   operation released to retry or to follow is not final.
//! - `elapsed_ms`: the whole milliseconds from the first poll to the end.
//! - `attempts`: the transport sends the operation made. A mock answer and a
//!   call refused before its first send record zero.
//! - `redirects`: the redirects the operation followed.
//! - `outcome`: one word of the closed vocabulary.
//!
//! Step 2 of the bounded-streaming plan adds the egress rows (2.T4). A call
//! its client's policy refuses records one `egress` completion with zero
//! attempts, from a bare call and from a served handler. An admitted call
//! records the sends its peer read. The policy's exception holds a sentinel.
//!
//! The rows run in a private child, because the capture they read is the
//! process's one global subscriber. Each row holds its own capture, so the
//! events it reads are its own. One capture per sentinel stays open for the
//! whole child and must end empty: that covers the completion events, the
//! retry and redirect events, and the events of the served refusal.
//!
//! 7.T2 reads the `traceparent` a real peer received. It runs with and without
//! the `otel` feature, and asserts the policy of each configuration.

use crate::common;
use crate::error_chain::assert_safe;
use crate::retry_upstream::{
    Answer, CHUNKED, Call, Pace, PacedUpstream, PeerEvent, Recorded, Recording, RecordingUpstream,
    RunnableDriver, ScriptedUpstream, StreamHead, StreamNext, TIMER_ROUNDING, WATCHDOG, answer,
    answered, assert_released, close, demand, failed_as, failure, must_settle, on_peers,
    paced_chunk, paced_end, paced_head, peer_runtime, read_out, required, saw_each, settle_wakeups,
    settled, watch_streams, within_watchdog, yielded_until,
};
use crate::untrusted_tls::{TlsPeerEvent, untrusted_tls_peer};

use camber::__private::ClientDnsEdge;
use camber::__private::client_dns_edge;
use camber::http;
use camber::http::Client;
use camber::http::ClientBody;
use camber::http::ClientDecode;
use camber::http::ClientEgress;
use camber::http::ClientError;
use camber::http::ClientInput;
use camber::http::ClientRedirect;
use camber::http::ClientRequest;
use camber::http::ClientResponse;
use camber::http::ClientStreamResponse;
use camber::http::ClientTimeout;
use camber::http::EgressPolicy;
use camber::http::RedirectPolicy;
use camber::http::mock;
use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;
use futures_util::future::Either;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The name every completion event carries.
const COMPLETED: &str = "http.client.completed";

/// The field a completion event records its duration under.
const ELAPSED_FIELD: &str = "elapsed_ms";

/// The status a completion event records when no final head arrived.
const NO_STATUS: &str = "none";

/// The host every loopback row's call begins at.
const LOOPBACK_HOST: &str = "127.0.0.1";

/// The host the mocked row's call begins at.
const MOCK_HOST: &str = "provider.mock.test";

/// What the general request rows send that no diagnostic may repeat.
const TOKEN_SECRET: &str = "token-sentinel-3b7e";
const HEADER_SECRET: &str = "tenant-sentinel-a41c";
const BODY_SECRET: &str = "request-body-sentinel-6d02";

/// What every row's request target carries.
const PATH_SECRET: &str = "path-sentinel-90fe";
const QUERY_SECRET: &str = "query-sentinel-12c8";
const FRAGMENT_SECRET: &str = "fragment-sentinel-e7a5";

/// What the peer's answers carry.
const REPLY_HEADER_SECRET: &str = "reply-header-sentinel-5c19";
const REPLY_BODY_SECRET: &str = "reply-body-sentinel-f083";

/// What the peer's `Location` values carry.
const LOCATION_QUERY_SECRET: &str = "location-sentinel-27bd";
const LOCATION_USER_SECRET: &str = "location-user-sentinel-b6e4";
const LOCATION_PASSWORD_SECRET: &str = "location-password-sentinel-08a3";

/// What the egress rows' policy holds in an exception no row's target uses.
const EXCEPTION_SECRET: &str = "exception-sentinel-4a9c";

/// The name the egress rows resolve through a controlled lookup.
const EGRESS_HOST: &str = "fetch.egress.test";

/// The address every egress row's peer listens on.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// One more address than a restricted operation checks.
const PAST_THE_VISIT_BOUND: [IpAddr; 65] = [LOOPBACK; 65];

/// Every sentinel a request, an answer, a redirect, or a policy of this
/// module carries.
const SECRETS: [&str; 12] = [
    EXCEPTION_SECRET,
    TOKEN_SECRET,
    HEADER_SECRET,
    BODY_SECRET,
    PATH_SECRET,
    QUERY_SECRET,
    FRAGMENT_SECRET,
    REPLY_HEADER_SECRET,
    REPLY_BODY_SECRET,
    LOCATION_QUERY_SECRET,
    LOCATION_USER_SECRET,
    LOCATION_PASSWORD_SECRET,
];

/// The first path segment of each scripted answer.
const ANSWER: &str = "/answer";
const MISSING: &str = "/missing";
const FLAKY: &str = "/flaky";
const BUSY: &str = "/busy";
const FOLLOWED: &str = "/followed";
const LOOPING: &str = "/looping";
const USERINFO: &str = "/userinfo";
const LARGE: &str = "/large";

/// The body length of the `LARGE` answer, and the ceiling it crosses.
const LARGE_BODY: usize = 64;
const SMALL_LIMIT: usize = 8;

/// The retry count of the rows that have retries to spare.
const SPARE_RETRIES: u32 = 2;

/// The base backoff of the rows that cross a delay on real time.
const CROSSED_BACKOFF: Duration = Duration::from_millis(1);

/// A client bound no row waits for.
const UNREACHED: Duration = Duration::from_secs(60);

/// The operation total of the row that expires under paused time.
const EXPIRING_TOTAL: Duration = Duration::from_secs(5);

const ONE_HOP: NonZeroUsize = NonZeroUsize::new(1).unwrap();
const TWO_HOPS: NonZeroUsize = NonZeroUsize::new(2).unwrap();

/// A request target at `base` that carries every URL sentinel.
///
/// `scripted` is the first path segment, which selects the peer's answer.
fn secret_url(base: &str, scripted: &str) -> String {
    format!("{base}{scripted}/{PATH_SECRET}?token={QUERY_SECRET}#{FRAGMENT_SECRET}")
}

/// A general request that carries both header sentinels and suppresses the
/// automatic trace context.
fn described<'a>(client: &'a Client, method: &str, url: &str) -> ClientRequest<'a> {
    client
        .request(method, url)
        .and_then(|request| {
            request.sensitive_header("authorization", &format!("Bearer {TOKEN_SECRET}"))
        })
        .and_then(|request| request.header("x-tenant", HEADER_SECRET))
        .unwrap_or_else(|error| panic!("{method} request was refused: {error:?}"))
        .trace_context(false)
}

/// A redirect to `location`.
fn redirect(location: &str) -> Vec<u8> {
    answer("302 Found", &[("Location", location)], b"")
}

/// A `Location` at the request's own host that holds a user name and a
/// password.
fn userinfo_location(request: &Recorded) -> String {
    let host = request
        .header_values("host")
        .first()
        .copied()
        .unwrap_or_default();
    format!("http://{LOCATION_USER_SECRET}:{LOCATION_PASSWORD_SECRET}@{host}{ANSWER}")
}

/// A body that is neither UTF-8 nor JSON, and holds the reply sentinel.
fn undecodable() -> Vec<u8> {
    [&[0xFF_u8][..], REPLY_BODY_SECRET.as_bytes()].concat()
}

/// Answer one request by the first segment of its target.
///
/// Each row binds its own peer, so the first connection is the row's first
/// attempt. A transient answer closes its connection, so the retry after it
/// arrives on the next one.
fn respond(request: &Recorded) -> Vec<u8> {
    let first_connection = request.connection == 0;
    match request.target() {
        target if target.starts_with(MISSING) => answer("404 Not Found", &[], &undecodable()),
        target if target.starts_with(BUSY) || (target.starts_with(FLAKY) && first_connection) => {
            answer("503 Service Unavailable", &[("Connection", "close")], b"")
        }
        target if target.starts_with(ANSWER) || target.starts_with(FLAKY) => answer(
            "200 OK",
            &[("X-Reply", REPLY_HEADER_SECRET)],
            REPLY_BODY_SECRET.as_bytes(),
        ),
        target if target.starts_with(FOLLOWED) => {
            redirect(&format!("{ANSWER}/landed?next={LOCATION_QUERY_SECRET}"))
        }
        target if target.starts_with(LOOPING) => redirect(target),
        target if target.starts_with(USERINFO) => redirect(&userinfo_location(request)),
        target if target.starts_with(LARGE) => answer("200 OK", &[], &[b'x'; LARGE_BODY]),
        _ => answer("500 Internal Server Error", &[], b"unscripted target"),
    }
}

/// What one completion event must record.
#[derive(Clone, Copy, Debug)]
struct Completion {
    method: &'static str,
    host: &'static str,
    status: &'static str,
    attempts: usize,
    redirects: usize,
    outcome: &'static str,
}

impl Completion {
    /// A loopback call that ended with no final head.
    const fn unanswered(method: &'static str, attempts: usize, outcome: &'static str) -> Self {
        Self {
            method,
            host: LOOPBACK_HOST,
            status: NO_STATUS,
            attempts,
            redirects: 0,
            outcome,
        }
    }

    /// A loopback call that ended at a final head with this status.
    const fn answered(method: &'static str, status: &'static str, attempts: usize) -> Self {
        Self {
            method,
            host: LOOPBACK_HOST,
            status,
            attempts,
            redirects: 0,
            outcome: "response",
        }
    }

    fn outcome(self, outcome: &'static str) -> Self {
        Self { outcome, ..self }
    }

    /// The same completion for a call that began at `host`.
    fn at(self, host: &'static str) -> Self {
        Self { host, ..self }
    }

    fn redirects(self, redirects: usize) -> Self {
        Self { redirects, ..self }
    }

    /// Every stated field, by the name production records it under.
    fn fields(&self) -> [(&'static str, String); 6] {
        [
            ("method", self.method.to_owned()),
            ("host", self.host.to_owned()),
            ("status", self.status.to_owned()),
            ("attempts", self.attempts.to_string()),
            ("redirects", self.redirects.to_string()),
            ("outcome", self.outcome.to_owned()),
        ]
    }
}

/// Require one event to record exactly `expected`, and a duration.
///
/// Each field is read at its boundary and must appear once. The event names a
/// host, so it may not also name a scheme: a URL would carry the path with it.
fn assert_event(row: &str, event: &str, expected: &Completion) {
    for (name, value) in expected.fields() {
        assert_eq!(
            common::field_occurrences(event, name),
            1,
            "{row}: the event must record {name} once: {event}"
        );
        common::assert_field_value(event, name, &value, row);
    }
    let elapsed = common::field_value(event, ELAPSED_FIELD)
        .unwrap_or_else(|| panic!("{row}: the event records no {ELAPSED_FIELD}: {event}"));
    assert!(
        elapsed.parse::<u64>().is_ok(),
        "{row}: {ELAPSED_FIELD} is {elapsed:?}, not a whole number: {event}"
    );
    assert!(
        !event.contains("://"),
        "{row}: the event names a URL: {event}"
    );
}

/// Require the capture to hold one completion event, recording `expected`.
fn assert_completion(row: &str, capture: &common::TraceCapture, expected: &Completion) {
    let events = capture.events();
    assert_eq!(
        events.len(),
        1,
        "{row}: one started operation records one completion: {events:?}"
    );
    assert_event(row, &events[0], expected);
}

/// The failure a row's call returned, safe in every rendering and source.
///
/// The peer's address is refused with the sentinels: an error that names
/// where it dialed has kept part of the URL.
fn safe_failure(
    row: &str,
    result: Result<ClientResponse, RuntimeError>,
    addr: SocketAddr,
) -> RuntimeError {
    safe_error(row, failure(row, result), Some(addr))
}

/// Every sentinel, and the address of the row's peer if it has one.
fn secrets(addr: Option<SocketAddr>) -> Box<[Box<str>]> {
    SECRETS
        .iter()
        .map(|secret| Box::from(*secret))
        .chain(addr.map(|addr| addr.to_string().into_boxed_str()))
        .collect()
}

/// Run one call under a capture of its own, bounded by the fixture watchdog.
async fn observe(
    row: &str,
    call: impl Future<Output = Result<ClientResponse, RuntimeError>>,
) -> (Result<ClientResponse, RuntimeError>, common::TraceCapture) {
    let capture = common::capture_events(COMPLETED);
    let result = within_watchdog(call).await;
    (required(row, result), capture)
}

/// Require the peer to have read as many requests as `expected` counts
/// attempts.
fn assert_peer_read(row: &str, recording: &Recording, expected: &Completion) {
    assert_eq!(
        recording.requests.len(),
        expected.attempts,
        "{row}: the peer read {:?}",
        recording.requests
    );
}

/// What one row's call returned, recorded, and sent.
struct Seen {
    result: Result<ClientResponse, RuntimeError>,
    capture: common::TraceCapture,
    recording: Recording,
    addr: SocketAddr,
}

impl Seen {
    /// Require one completion recording `expected`, and a peer that read as
    /// many requests as the event counts attempts.
    fn assert_completed(&self, row: &str, expected: &Completion) {
        assert_completion(row, &self.capture, expected);
        assert_peer_read(row, &self.recording, expected);
        assert_eq!(
            self.recording.starts as usize, expected.attempts,
            "{row}: the peer saw {} request starts",
            self.recording.starts
        );
    }
}

/// Run one call against a scripted peer of its own.
///
/// `call` is given the peer's base URL. The peer is probed, stopped, and
/// joined before anything is returned. The probe is accepted after every
/// connection the call made, so the recording is complete, and a refused call
/// that dialed is counted.
async fn drive<F, Fut>(row: &str, call: F) -> Seen
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<ClientResponse, RuntimeError>>,
{
    let upstream = RecordingUpstream::bind_responder(respond).await;
    let addr = upstream.addr();
    let capture = common::capture_events(COMPLETED);
    let result = within_watchdog(call(upstream.url(""))).await;
    let recording = upstream.finish_probed(row).await;
    Seen {
        result: required(row, result),
        capture,
        recording,
        addr,
    }
}

/// Require a 200 answer and one `response` completion under `method`.
fn assert_answered(row: &str, seen: &Seen, method: &'static str) {
    match &seen.result {
        Ok(response) => assert_eq!(response.status(), 200, "{row}: final status"),
        Err(error) => panic!("{row}: the call failed: {error:?}"),
    }
    seen.assert_completed(row, &Completion::answered(method, "200", 1));
}

/// Require a recorded request to have carried `method`, both header
/// sentinels, the body sentinel, and the target sentinels.
fn assert_request_sentinels(row: &str, sent: &Recorded, method: &str) {
    assert_eq!(sent.method(), method, "{row}: the method on the wire");
    assert_eq!(
        *sent.header_values("authorization"),
        [format!("Bearer {TOKEN_SECRET}").as_str()],
        "{row}: the sensitive header on the wire"
    );
    assert_eq!(
        *sent.header_values("x-tenant"),
        [HEADER_SECRET],
        "{row}: the ordinary header on the wire"
    );
    assert_eq!(&*sent.body, BODY_SECRET.as_bytes(), "{row}: the body sent");
    assert!(
        sent.target().contains(PATH_SECRET) && sent.target().contains(QUERY_SECRET),
        "{row}: the target on the wire is {}",
        sent.target()
    );
}

/// Require the general row's request and answer to have carried the
/// sentinels, so their absence from every diagnostic is a redaction.
fn assert_sentinels_travelled(row: &str, seen: &Seen) {
    assert_request_sentinels(row, &seen.recording.requests[0], "PURGE");
    let answer = seen.result.as_ref().expect("an answered general request");
    assert_eq!(
        answer.header("x-reply"),
        Some(REPLY_HEADER_SECRET.as_bytes()),
        "{row}: the answer's header"
    );
    assert_eq!(
        answer.body_bytes(),
        REPLY_BODY_SECRET.as_bytes(),
        "{row}: the answer's body"
    );
}

/// Every way into the client starts one operation and records it once.
async fn every_ingress_completes_once() {
    let row = "free verb";
    let seen = drive(row, |base| async move {
        http::get(&secret_url(&base, ANSWER)).await
    })
    .await;
    assert_answered(row, &seen, "GET");

    let row = "free form helper";
    let seen = drive(row, |base| async move {
        http::put_form(&secret_url(&base, ANSWER), BODY_SECRET).await
    })
    .await;
    assert_answered(row, &seen, "PUT");

    let row = "reusable verb";
    let seen = drive(row, |base| async move {
        http::client()
            .post(&secret_url(&base, ANSWER), BODY_SECRET)
            .await
    })
    .await;
    assert_answered(row, &seen, "POST");

    let row = "reusable JSON helper";
    let seen = drive(row, |base| async move {
        http::client()
            .patch_json(&secret_url(&base, ANSWER), BODY_SECRET)
            .await
    })
    .await;
    assert_answered(row, &seen, "PATCH");

    let row = "general request, extension method";
    let seen = drive(row, |base| async move {
        let client = http::client();
        described(&client, "PURGE", &secret_url(&base, ANSWER))
            .body(BODY_SECRET)
            .send()
            .await
    })
    .await;
    assert_answered(row, &seen, "OTHER");
    assert_sentinels_travelled(row, &seen);
}

/// A mocked call enters the operation and records one completion with no
/// transport send.
async fn mocked_call_completes_once() {
    let row = "mocked call";
    let url = secret_url(&format!("http://{MOCK_HOST}"), ANSWER);
    let canned = Response::text(200, REPLY_BODY_SECRET).expect("a valid mock status");
    let registered = mock::http(&url).returns(canned);

    let (result, capture) = observe(row, async {
        let client = http::client();
        described(&client, "GET", &url).send().await
    })
    .await;

    match result {
        Ok(response) => assert_eq!(response.status(), 200, "{row}: final status"),
        Err(error) => panic!("{row}: the call failed: {error:?}"),
    }
    registered.assert_called_once();
    assert_completion(
        row,
        &capture,
        &Completion::answered("GET", "200", 0).at(MOCK_HOST),
    );
}

/// A failed local conversion of a returned answer records nothing more.
///
/// The capture stays open across the three conversions, so an event one of
/// them emitted would be the second in it.
async fn local_conversions_record_nothing() {
    let row = "local conversions";
    let Seen {
        result,
        capture,
        recording,
        addr,
    } = drive(row, |base| async move {
        let client = http::client();
        described(&client, "GET", &secret_url(&base, MISSING))
            .send()
            .await
    })
    .await;
    let response = answered(row, result);

    let text = response.text().map(str::to_owned);
    let decoded = response.json::<serde_json::Value>();
    let converted = response.error_for_status().map(|kept| kept.status());

    assert!(
        matches!(
            text,
            Err(RuntimeError::Client(ClientError::Decode(
                ClientDecode::Utf8
            )))
        ),
        "{row}: text returned {text:?}"
    );
    assert!(
        matches!(
            decoded,
            Err(RuntimeError::Client(ClientError::Decode(
                ClientDecode::Json
            )))
        ),
        "{row}: json returned {decoded:?}"
    );
    assert!(
        matches!(
            converted,
            Err(RuntimeError::Client(ClientError::Status(404)))
        ),
        "{row}: error_for_status returned {converted:?}"
    );
    let private = secrets(Some(addr));
    for error in [text.err(), decoded.err(), converted.err()]
        .into_iter()
        .flatten()
    {
        assert_safe(&error, &private, row);
    }
    assert_completion(row, &capture, &Completion::answered("GET", "404", 1));
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// A client with retries to spare and a backoff real time crosses.
fn retrying_client() -> Client {
    http::client()
        .retries(SPARE_RETRIES)
        .backoff(CROSSED_BACKOFF)
}

/// The attempt count is the sends the peer read, with retries and without an
/// answer.
async fn retries_are_counted_as_attempts() {
    let row = "one retry, then an answer";
    let seen = drive(row, |base| async move {
        let client = retrying_client();
        described(&client, "GET", &secret_url(&base, FLAKY))
            .send()
            .await
    })
    .await;
    assert!(seen.result.is_ok(), "{row}: returned {:?}", seen.result);
    seen.assert_completed(row, &Completion::answered("GET", "200", 2));

    let row = "every retry spent on a transient status";
    let seen = drive(row, |base| async move {
        let client = retrying_client();
        described(&client, "GET", &secret_url(&base, BUSY))
            .send()
            .await
    })
    .await;
    assert!(seen.result.is_ok(), "{row}: returned {:?}", seen.result);
    seen.assert_completed(
        row,
        &Completion::answered("GET", "503", SPARE_RETRIES as usize + 1),
    );

    let row = "every retry spent on a refused dial";
    let addr = common::refused_loopback_addr();
    let url = secret_url(&format!("http://{addr}"), ANSWER);
    let (result, capture) = observe(row, async {
        let client = retrying_client();
        described(&client, "GET", &url).send().await
    })
    .await;
    let error = safe_failure(row, result, addr);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Connect)),
        "{row}: returned {error:?}"
    );
    assert_completion(
        row,
        &capture,
        &Completion::unanswered("GET", SPARE_RETRIES as usize + 1, "connect"),
    );
}

/// One GET under a same-origin policy of `max_hops`.
async fn redirected(row: &str, scripted: &'static str, max_hops: NonZeroUsize) -> Seen {
    drive(row, |base| async move {
        let client = http::client();
        described(&client, "GET", &secret_url(&base, scripted))
            .redirect_policy(RedirectPolicy::SameOrigin { max_hops })
            .send()
            .await
    })
    .await
}

/// Require a refused redirect of this kind, safe in every rendering.
fn assert_refused(row: &str, seen: Seen, kind: ClientRedirect, expected: &Completion) {
    seen.assert_completed(row, expected);
    let error = safe_failure(row, seen.result, seen.addr);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Redirect(refused)) if refused == kind),
        "{row}: returned {error:?} instead of {kind:?}"
    );
}

/// A followed redirect is counted, and a refused one ends the operation with
/// no final head.
async fn redirects_are_counted_and_refusals_recorded() {
    let row = "one followed redirect";
    let seen = redirected(row, FOLLOWED, TWO_HOPS).await;
    assert!(seen.result.is_ok(), "{row}: returned {:?}", seen.result);
    seen.assert_completed(row, &Completion::answered("GET", "200", 2).redirects(1));

    let row = "a Location with userinfo";
    let seen = redirected(row, USERINFO, TWO_HOPS).await;
    assert_refused(
        row,
        seen,
        ClientRedirect::InvalidLocation,
        &Completion::unanswered("GET", 1, "redirect"),
    );

    let row = "a redirect past the hop limit";
    let seen = redirected(row, LOOPING, ONE_HOP).await;
    assert_refused(
        row,
        seen,
        ClientRedirect::HopLimit,
        &Completion::unanswered("GET", 2, "redirect").redirects(1),
    );
}

/// An answer above its ceiling is a final head whose body was refused.
async fn crossed_limit_is_recorded() {
    let row = "an answer above the response limit";
    let seen = drive(row, |base| async move {
        let client = http::client();
        described(&client, "GET", &secret_url(&base, LARGE))
            .response_limit(SMALL_LIMIT)
            .expect("a nonzero response limit")
            .send()
            .await
    })
    .await;

    seen.assert_completed(
        row,
        &Completion::answered("GET", "200", 1).outcome("response-too-large"),
    );
    let error = safe_failure(row, seen.result, seen.addr);
    assert!(
        matches!(
            error,
            RuntimeError::Client(ClientError::ResponseTooLarge { limit: SMALL_LIMIT })
        ),
        "{row}: returned {error:?}"
    );
}

/// A description refused at send starts the operation and sends nothing.
async fn send_time_refusal_records_zero_attempts() {
    let row = "TRACE with a body";
    let seen = drive(row, |base| async move {
        let client = http::client();
        described(&client, "TRACE", &secret_url(&base, ANSWER))
            .body(BODY_SECRET)
            .send()
            .await
    })
    .await;

    seen.assert_completed(row, &Completion::unanswered("TRACE", 0, "invalid-request"));
    assert_eq!(
        seen.recording.connections, 0,
        "{row}: a refused description dialed its target"
    );
    let error = safe_failure(row, seen.result, seen.addr);
    assert!(
        matches!(
            error,
            RuntimeError::Client(ClientError::InvalidRequest(ClientInput::Body))
        ),
        "{row}: returned {error:?}"
    );
}

/// A restricted policy with one exception no row's target uses.
///
/// The exception holds a sentinel, so a policy that reached an event or an
/// error would be seen there.
fn guarded_policy() -> EgressPolicy {
    EgressPolicy::public_internet()
        .allow_destination(
            &format!("https://{EXCEPTION_SECRET}.egress.test:8443"),
            IpAddr::V4(Ipv4Addr::new(10, 20, 30, 40)),
        )
        .unwrap_or_else(|error| panic!("the guarded exception was refused: {error:?}"))
}

/// The peer at `base`, addressed by the egress rows' name.
fn named(base: &str) -> String {
    base.replacen(LOOPBACK_HOST, EGRESS_HOST, 1)
}

/// [`guarded_policy`], which also admits the name at the peer at `base`.
fn admitting(base: &str) -> EgressPolicy {
    guarded_policy()
        .allow_destination(&named(base), LOOPBACK)
        .unwrap_or_else(|error| panic!("the peer's exception was refused: {error:?}"))
}

/// Require one `egress` completion with no send, a peer nothing dialed, and
/// the refusal `reason`, safe in every rendering.
fn assert_egress_refusal(row: &str, seen: Seen, reason: ClientEgress, host: &'static str) {
    seen.assert_completed(row, &Completion::unanswered("GET", 0, "egress").at(host));
    assert_eq!(
        seen.recording.connections, 0,
        "{row}: a refused destination was dialed"
    );
    let error = safe_failure(row, seen.result, seen.addr);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Egress(found)) if found == reason),
        "{row}: returned {error:?} instead of {reason:?}"
    );
}

/// A destination the policy refuses starts the operation and sends nothing.
/// Each reason records one `egress` completion with zero attempts.
async fn egress_refusals_record_zero_attempts() {
    let row = "egress: restricted literal";
    let seen = drive(row, |base| async move {
        let client: Client = http::client().egress_policy(guarded_policy());
        described(&client, "GET", &secret_url(&base, ANSWER))
            .send()
            .await
    })
    .await;
    assert_egress_refusal(row, seen, ClientEgress::Address, LOOPBACK_HOST);

    let row = "egress: HTTP under an HTTPS-only policy";
    let seen = drive(row, |base| async move {
        let client: Client = http::client().egress_policy(guarded_policy().https_only());
        described(&client, "GET", &secret_url(&base, ANSWER))
            .send()
            .await
    })
    .await;
    assert_egress_refusal(row, seen, ClientEgress::Scheme, LOOPBACK_HOST);

    let row = "egress: one address more than an operation checks";
    let seen = drive(row, |base| async move {
        let edge = ClientDnsEdge::new();
        edge.answer(&PAST_THE_VISIT_BOUND);
        let restricted: Client = http::client().egress_policy(admitting(&base));
        let client: Client = client_dns_edge(restricted, &edge);
        described(&client, "GET", &secret_url(&named(&base), ANSWER))
            .send()
            .await
    })
    .await;
    assert_egress_refusal(row, seen, ClientEgress::ResolutionLimit, EGRESS_HOST);
}

/// An admitted destination records the sends its peer read, the retry
/// included, and the name the call began at.
async fn admitted_egress_counts_peer_attempts() {
    let row = "egress: excepted name, one retry";
    let seen = drive(row, |base| async move {
        let edge = ClientDnsEdge::new();
        edge.answer(&[LOOPBACK]);
        let restricted: Client = retrying_client().egress_policy(admitting(&base));
        let client: Client = client_dns_edge(restricted, &edge);
        described(&client, "GET", &secret_url(&named(&base), FLAKY))
            .send()
            .await
    })
    .await;
    assert!(seen.result.is_ok(), "{row}: returned {:?}", seen.result);
    seen.assert_completed(row, &Completion::answered("GET", "200", 2).at(EGRESS_HOST));
}

/// A value whose serialization fails with a message that holds a sentinel.
struct Unencodable;

impl serde::Serialize for Unencodable {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom(BODY_SECRET))
    }
}

/// Require a refusal with this category, safe in every rendering.
///
/// `addr` is the row's peer, if the refused description named one.
fn assert_safe_refusal(
    row: &str,
    refused: Result<(), RuntimeError>,
    expected: ClientError,
    addr: Option<SocketAddr>,
) {
    match refused {
        Err(error) => {
            assert!(
                matches!(error, RuntimeError::Client(category) if category == expected),
                "{row}: returned {error:?} instead of {expected:?}"
            );
            assert_safe(&error, &secrets(addr), row);
        }
        Ok(()) => panic!("{row}: the description was not refused"),
    }
}

/// A builder refusal and an unpolled send start no operation, so neither
/// records an event or dials.
async fn unstarted_calls_record_nothing() {
    let row = "unstarted calls";
    let upstream = RecordingUpstream::bind_responder(respond).await;
    let url = secret_url(&upstream.url(""), ANSWER);
    let capture = common::capture_events(COMPLETED);
    let client = http::client();

    let method = client.request("CONNECT", &url).map(drop);
    let reserved = described(&client, "POST", &url)
        .header("host", HEADER_SECRET)
        .map(drop);
    let encoded = described(&client, "POST", &url)
        .json(&Unencodable)
        .map(drop);
    let limited = described(&client, "POST", &url).response_limit(0).map(drop);
    drop(described(&client, "POST", &url).body(BODY_SECRET).send());

    let events = capture.events();
    let recording = upstream.finish_probed(row).await;

    assert_safe_refusal(
        "a refused method",
        method,
        ClientError::InvalidRequest(ClientInput::Method),
        None,
    );
    assert_safe_refusal(
        "a reserved header",
        reserved,
        ClientError::InvalidRequest(ClientInput::ReservedHeader),
        None,
    );
    assert_safe_refusal("an unencodable body", encoded, ClientError::Encode, None);
    assert_safe_refusal(
        "a zero response limit",
        limited,
        ClientError::InvalidRequest(ClientInput::ResponseLimit),
        None,
    );
    assert!(
        events.is_empty(),
        "{row}: a call that never started recorded {events:?}"
    );
    assert_eq!(
        recording.connections, 0,
        "{row}: a call that never started dialed its target"
    );
}

/// A body cut short of its declared length ends a final head as a transport
/// failure.
async fn truncated_body_is_recorded() {
    let row = "a truncated body";
    let mut upstream = ScriptedUpstream::bind(&[Answer::Truncate]).await;
    let addr = upstream.addr();
    let url = secret_url(&upstream.url(""), ANSWER);

    let (result, capture) = observe(row, async {
        let client = http::client();
        described(&client, "POST", &url)
            .body(BODY_SECRET)
            .send()
            .await
    })
    .await;
    let started = upstream.saw(PeerEvent::Started(0)).await;
    let cut = upstream.saw(PeerEvent::Truncated(0)).await;
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), row).await;

    assert_eq!(started, Ok(()), "{row}: the peer never read the request");
    assert_eq!(cut, Ok(()), "{row}: the peer never cut its answer short");
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
    let error = safe_failure(row, result, addr);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Transport)),
        "{row}: returned {error:?}"
    );
    assert_completion(
        row,
        &capture,
        &Completion::answered("POST", "200", 1).outcome("transport"),
    );
}

/// A certificate nothing trusts ends the one attempt before any request.
async fn untrusted_certificate_is_recorded() {
    let row = "an untrusted certificate";
    let mut peer = untrusted_tls_peer().await;
    let addr = peer.addr();
    let url = secret_url(&format!("https://{addr}"), ANSWER);

    let (result, capture) = observe(row, async {
        let client = http::client();
        described(&client, "POST", &url)
            .body(BODY_SECRET)
            .send()
            .await
    })
    .await;
    let seen = within_watchdog(peer.recv()).await;
    peer.finish(row, within_watchdog).await;

    assert_eq!(
        seen,
        Some(Some(TlsPeerEvent::HandshakeRefused)),
        "{row}: the client must refuse the handshake before it sends a request"
    );
    let error = safe_failure(row, result, addr);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Tls)),
        "{row}: returned {error:?}"
    );
    assert_completion(row, &capture, &Completion::unanswered("POST", 1, "tls"));
}

/// Start one GET at a peer that holds its head, as a task of its own.
fn spawn_held(url: String, total: Duration) -> Call {
    tokio::spawn(async move {
        let client = http::client().total_timeout(total);
        described(&client, "GET", &url).send().await
    })
}

/// A head that never arrives ends at the operation total.
///
/// Pause before the request arms its timer. After the peer acknowledges the
/// stall, advance past the rounded deadline and await the client's result.
async fn expired_operation_is_recorded() {
    let row = "an expired operation";
    tokio::time::pause();
    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallHead]).await;
    let addr = upstream.addr();
    let capture = common::capture_events(COMPLETED);
    let call = spawn_held(secret_url(&upstream.url(""), ANSWER), EXPIRING_TOTAL);

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], row)
        .await;
    tokio::time::advance(EXPIRING_TOTAL + TIMER_ROUNDING).await;
    let result = settled(call).await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let starts = upstream.finish(driver, row).await;
    tokio::time::resume();

    let error = safe_failure(row, required(row, result), addr);
    assert!(
        matches!(
            error,
            RuntimeError::Client(ClientError::Timeout(ClientTimeout::Operation))
        ),
        "{row}: returned {error:?}"
    );
    assert_released(released, row);
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
    assert_completion(row, &capture, &Completion::unanswered("GET", 1, "timeout"));
}

/// A pending call that is dropped records its own end.
async fn abandoned_operation_is_recorded() {
    let row = "an abandoned operation";
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallHead]).await;
    let capture = common::capture_events(COMPLETED);
    let call = spawn_held(secret_url(&upstream.url(""), ANSWER), UNREACHED);

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], row)
        .await;
    let pending = capture.len();
    call.abort();
    let joined = within_watchdog(call).await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let starts = upstream.finish(RunnableDriver::start(), row).await;

    assert_eq!(
        pending, 0,
        "{row}: a pending operation had already recorded its end"
    );
    assert!(
        matches!(&joined, Some(Err(error)) if error.is_cancelled()),
        "{row}: the dropped call was not cancelled"
    );
    assert_released(released, row);
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
    assert_completion(
        row,
        &capture,
        &Completion::unanswered("GET", 1, "abandoned"),
    );
}

/// A call polled with no executor starts the operation and ends it at once.
///
/// The row runs before any runtime exists.
fn unexecuted_call_is_recorded() {
    assert_unexecuted("a call polled with no executor", |client, url| {
        described(client, "GET", url)
            .send()
            .now_or_never()
            .map(|sent| sent.map(|answer| answer.status()))
    });
}

/// Run one refused GET at a peer nothing may dial, and require one completion
/// with this `outcome`, no attempt, and nothing dialed.
///
/// `refuse` is given the URL and the peer's address. It sends, and requires
/// the refusal it was returned. The listener is never accepted on, so an
/// empty backlog proves the call dialed nothing.
fn assert_refused_undialed(
    row: &str,
    outcome: &'static str,
    refuse: impl FnOnce(&str, SocketAddr),
) {
    let listener = common::UndialedListener::bind();
    let url = secret_url(&format!("http://{}", listener.addr()), ANSWER);
    let capture = common::capture_events(COMPLETED);

    refuse(&url, listener.addr());

    listener.assert_undialed(row);
    assert_completion(row, &capture, &Completion::unanswered("GET", 0, outcome));
}

/// Poll one described GET once with no executor, and require `NoRuntime`,
/// one `no-runtime` completion, and nothing dialed.
///
/// `poll` sends through the terminal under proof and reports the status of
/// an answer.
fn assert_unexecuted(
    row: &str,
    poll: impl FnOnce(&Client, &str) -> Option<Result<u16, RuntimeError>>,
) {
    assert_refused_undialed(row, "no-runtime", |url, _| {
        let client = http::client();
        let polled = poll(&client, url);
        assert!(
            matches!(polled, Some(Err(RuntimeError::NoRuntime))),
            "{row}: returned {polled:?}"
        );
    });
}

/// Every row that needs no Camber runtime, on one bare Tokio runtime.
fn bare_rows() {
    common::block_on_detached(async {
        every_ingress_completes_once().await;
        mocked_call_completes_once().await;
        local_conversions_record_nothing().await;
        retries_are_counted_as_attempts().await;
        redirects_are_counted_and_refusals_recorded().await;
        crossed_limit_is_recorded().await;
        send_time_refusal_records_zero_attempts().await;
        egress_refusals_record_zero_attempts().await;
        admitted_egress_counts_peer_attempts().await;
        unstarted_calls_record_nothing().await;
        truncated_body_is_recorded().await;
        untrusted_certificate_is_recorded().await;
        abandoned_operation_is_recorded().await;
        // Last, because it is the one row that holds the clock still.
        expired_operation_is_recorded().await;
    });
}

/// The route whose handler returns its refused call as its own failure.
const REFUSED_ROUTE: &str = "/outbound-refused";

/// The route whose handler returns a call its egress policy refused.
const EGRESS_ROUTE: &str = "/outbound-egress-refused";

/// The route whose handler awaits a call the peer holds.
const HELD_ROUTE: &str = "/outbound-held";

/// The name the journey router's mapper records its refusals under.
const JOURNEY_MAPPER: &str = "outbound-journey";

/// Answer with the status of a call through `client` that is refused.
///
/// The target is a loopback literal nothing listens on. A dial is refused
/// there, and a restricted egress policy refuses it with no lookup and no
/// peer. The handler returns the failure, so the server maps it.
async fn refused_call(client: Client) -> Result<Response, RuntimeError> {
    let url = secret_url(
        &format!("http://{}", common::refused_loopback_addr()),
        ANSWER,
    );
    let answer = described(&client, "POST", &url)
        .body(BODY_SECRET)
        .send()
        .await?;
    Response::empty(answer.status())
}

/// A router with the three outbound routes, behind a mapper that records what
/// it was handed.
///
/// The two refused routes return their call's failure, so the server maps
/// each. The held route reports what its call returned, so the row reads the
/// result the handler was given and not the answer the inbound peer got.
fn journey_router(
    held_url: String,
    ends: Sender<Result<u16, RuntimeError>>,
    journal: &common::Journal,
) -> Router {
    let mut router = Router::new();
    router.get(EGRESS_ROUTE, |_req: &Request| {
        refused_call(http::client().egress_policy(guarded_policy()))
    });
    router.get(REFUSED_ROUTE, |_req: &Request| refused_call(http::client()));

    let held_url: Arc<str> = held_url.into();
    router.get(HELD_ROUTE, move |_req: &Request| {
        let url = Arc::clone(&held_url);
        let ends = ends.clone();
        async move {
            let client = http::client().total_timeout(UNREACHED);
            let returned = described(&client, "GET", &url)
                .send()
                .await
                .map(|answer| answer.status());
            let status = returned.as_ref().map_or(502, |_| 200);
            // A closed receiver means the row has already finished.
            let _ = ends.send(returned);
            Response::empty(status)
        }
    });

    router.rejection_mapper(common::recording_mapper(journal, JOURNEY_MAPPER))
}

/// Everything the served rows observed before their runtime returned.
struct Served {
    /// What the inbound peer of the refused route was answered.
    refused: std::io::Result<common::HttpResponse>,
    /// What the inbound peer of the egress-refused route was answered.
    egress: std::io::Result<common::HttpResponse>,
    /// Whether the peer confirmed the held call was parked at its head.
    reached: Result<(), String>,
    /// What the held call returned to its handler, or `None` if it never
    /// reported.
    end: Option<Result<u16, RuntimeError>>,
    /// Whether the client closed the held attempt's transport.
    released: Result<(), String>,
}

/// Drive the two refused routes in order, then hold a call and request
/// shutdown.
///
/// Nothing is asserted here. The row asserts after the runtime has returned
/// and the peer is released. The held route's inbound peer stays open until
/// the handler has reported, so its close cannot be what ended the call.
fn serve_rows(
    addr: SocketAddr,
    peers: &tokio::runtime::Runtime,
    upstream: &mut ScriptedUpstream,
    ended: &std::sync::mpsc::Receiver<Result<u16, RuntimeError>>,
) -> Served {
    let refused = common::request(addr, "GET", REFUSED_ROUTE, &[], &[], common::WIRE_TIMEOUT);
    let egress = common::request(addr, "GET", EGRESS_ROUTE, &[], &[], common::WIRE_TIMEOUT);
    let inbound = std::thread::spawn(move || {
        common::request(addr, "GET", HELD_ROUTE, &[], &[], common::WIRE_TIMEOUT)
    });
    let reached = on_peers(
        peers,
        saw_each(upstream, &[PeerEvent::Started(0), PeerEvent::Stalled(0)]),
    );
    runtime::request_shutdown();
    let end = ended.recv_timeout(WATCHDOG).ok();
    let released = on_peers(peers, upstream.saw(PeerEvent::Released(0)));
    // The inbound answer is not this row's evidence: shutdown may close the
    // connection before the handler's answer is written.
    let _ = inbound.join();
    Served {
        refused,
        egress,
        reached,
        end,
        released,
    }
}

/// The served row whose call a dial refused, and the one whose call the
/// handler's egress policy refused.
const REFUSED_ROW: &str = "a handler's refused call";
const EGRESS_ROW: &str = "a handler's egress-refused call";

/// Require each inbound peer and the mapper to have been given the fixed
/// internal refusal, with no sentinel in either.
///
/// The requests ran one after the other, so the journal holds one record for
/// each, in that order. A refusal that reached no mapper is a missing record.
fn assert_mapped_refusals(
    refusals: [(&str, std::io::Result<common::HttpResponse>); 2],
    journal: &common::Journal,
) {
    let mapped = common::drain(journal).into_vec();
    assert_eq!(
        mapped.len(),
        refusals.len(),
        "each refused call invokes the mapper once: {mapped:?}"
    );
    for ((row, refused), observed) in refusals.into_iter().zip(mapped) {
        let response = refused.unwrap_or_else(|error| panic!("{row}: never answered: {error}"));
        assert_eq!(response.status, 500, "{row}: wire status");
        assert_eq!(&*response.text(), common::REDACTED_BODY, "{row}: wire body");
        common::assert_no_private_text(&response, &SECRETS, row);
        common::assert_internal_refusal_record(&observed, JOURNEY_MAPPER, &SECRETS, row);
    }
}

/// The rows that need a Camber runtime: a served handler's refused call, one
/// its egress policy refused, and a held call ended by runtime shutdown.
///
/// The peer runs on a runtime of its own, so the runtime the row shuts down
/// owns nothing of the peer that observes it.
fn served_rows() {
    let row = "a handler's call, held at shutdown";
    let peers = peer_runtime();
    let mut upstream = on_peers(&peers, ScriptedUpstream::bind(&[Answer::StallHead]));
    let (ends, ended) = channel();
    let journal = common::journal();
    let router = journey_router(secret_url(&upstream.url(""), ANSWER), ends, &journal);
    let capture = common::capture_events(COMPLETED);

    let served = common::test_runtime()
        .run(|| serve_rows(common::spawn_server(router), &peers, &mut upstream, &ended))
        .expect("the fixture runtime ran to completion");
    let starts = on_peers(&peers, async {
        upstream.finish(RunnableDriver::start(), row).await
    });

    assert_mapped_refusals(
        [(REFUSED_ROW, served.refused), (EGRESS_ROW, served.egress)],
        &journal,
    );
    if let Err(unreached) = served.reached {
        panic!("{row}: the call was never held: {unreached}");
    }
    assert!(
        matches!(served.end, Some(Err(RuntimeError::Cancelled))),
        "{row}: the handler's call returned {:?}",
        served.end
    );
    assert_released(served.released, row);
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");

    let events = capture.events();
    assert_eq!(
        events.len(),
        3,
        "the three served calls record one completion each: {events:?}"
    );
    assert_event(
        REFUSED_ROW,
        &events[0],
        &Completion::unanswered("POST", 1, "connect"),
    );
    assert_event(
        EGRESS_ROW,
        &events[1],
        &Completion::unanswered("POST", 0, "egress"),
    );
    assert_event(
        row,
        &events[2],
        &Completion::unanswered("GET", 1, "cancelled"),
    );
}

/// One capture per sentinel, open for the whole child.
///
/// A capture keeps only the events that hold its needle, so each stays empty
/// unless production leaks, and no transcript budget is spent on the events
/// of a healthy run.
fn watch_for_secrets() -> Box<[(&'static str, common::TraceCapture)]> {
    SECRETS
        .into_iter()
        .map(|secret| (secret, common::capture_events(secret)))
        .collect()
}

/// Require that no event of the child held a sentinel.
fn assert_no_secret_was_recorded(watched: &[(&'static str, common::TraceCapture)]) {
    for (secret, capture) in watched {
        assert!(
            capture.is_empty() && !capture.truncated(),
            "production recorded the sentinel {secret:?} in {} event(s)",
            capture.len()
        );
    }
}

/// Every 7.T1 row, in the private child.
fn assert_completions_are_once_and_secret_free() {
    let watched = watch_for_secrets();
    unexecuted_call_is_recorded();
    bare_rows();
    served_rows();
    assert_no_secret_was_recorded(&watched);
}

/// How long the private child may take to drive every row.
///
/// Every row is bounded by the fixture watchdog, and none waits for it on a
/// healthy run. This bounds a child that hangs outside any row.
const COMPLETION_CHILD_BOUND: Duration = Duration::from_secs(120);

/// 7.T1
#[test]
fn outbound_completion_is_once_and_secret_free() {
    common::run_in_child(
        "outbound_client::outbound_completion_is_once_and_secret_free",
        "outbound-completion-events",
        "OUTBOUND_COMPLETION_EVENTS_COMPLETE",
        COMPLETION_CHILD_BOUND,
        assert_completions_are_once_and_secret_free,
    );
}

/// The header a trace context travels in.
const TRACEPARENT: &str = "traceparent";

/// A `traceparent` the application writes itself.
const EXPLICIT_TRACEPARENT: &str = "00-11111111111111111111111111111111-2222222222222222-01";

/// An application header every trace row sends, which no policy may remove.
const APPLICATION_HEADER: &str = "x-application";
const APPLICATION_VALUE: &str = "kept";

/// The index of the first sibling row's target. The policy matrix ends well
/// below it.
const SIBLING_INDEX: usize = 100;

/// Whether one request states its trace policy, and which.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Policy {
    /// The request never calls `trace_context`.
    Default,
    Enabled,
    Disabled,
}

const POLICIES: [Policy; 3] = [Policy::Default, Policy::Enabled, Policy::Disabled];

/// One request of the trace policy matrix.
#[derive(Clone, Copy, Debug)]
struct Case {
    policy: Policy,
    /// Whether the application writes [`EXPLICIT_TRACEPARENT`] itself.
    explicit: bool,
    /// Whether the peer answers the first send with a same-origin redirect.
    redirected: bool,
}

/// Every policy, with and without an explicit header, with and without a
/// same-origin redirect.
fn cases() -> impl Iterator<Item = Case> {
    POLICIES.into_iter().flat_map(|policy| {
        [false, true].into_iter().flat_map(move |explicit| {
            [false, true].into_iter().map(move |redirected| Case {
                policy,
                explicit,
                redirected,
            })
        })
    })
}

/// The three requests one handler sends at once through one client.
const SIBLINGS: [Case; 3] = [
    Case {
        policy: Policy::Default,
        explicit: false,
        redirected: false,
    },
    Case {
        policy: Policy::Disabled,
        explicit: false,
        redirected: false,
    },
    Case {
        policy: Policy::Enabled,
        explicit: true,
        redirected: false,
    },
];

/// What the peer must have received as `traceparent`.
#[derive(Debug)]
enum Sent<'a> {
    /// No `traceparent` at all.
    Nothing,
    /// The application's own value, once and unchanged.
    Explicit,
    /// One generated value in this trace.
    Trace(&'a str),
}

impl Case {
    /// The target prefix that names row `index` on the wire.
    fn prefix(index: usize) -> String {
        format!("/trace/{index}/")
    }

    /// The URL of row `index`: one the peer answers, or one it redirects.
    fn url(self, base: &str, index: usize) -> String {
        let leaf = match self.redirected {
            true => "hop",
            false => "first",
        };
        format!("{base}{}{leaf}", Self::prefix(index))
    }

    /// How many requests the peer reads for this row.
    fn sends(self) -> usize {
        match self.redirected {
            true => 2,
            false => 1,
        }
    }

    /// What each of this row's requests must carry, under the trace the
    /// caller's task holds, if it holds one.
    ///
    /// An explicit value always wins. Without one, only a disabled policy
    /// stops the ambient trace from being sent.
    fn sent(self, ambient: Option<&str>) -> Sent<'_> {
        match (self.explicit, self.policy, ambient) {
            (true, _, _) => Sent::Explicit,
            (false, Policy::Disabled, _) | (false, _, None) => Sent::Nothing,
            (false, Policy::Default | Policy::Enabled, Some(trace)) => Sent::Trace(trace),
        }
    }
}

/// Describe row `index` through the public request builder.
fn case_request<'a>(
    client: &'a Client,
    base: &str,
    index: usize,
    case: Case,
) -> Result<ClientRequest<'a>, RuntimeError> {
    let mut request = client
        .request("GET", &case.url(base, index))?
        .header(APPLICATION_HEADER, APPLICATION_VALUE)?
        .redirect_policy(RedirectPolicy::SameOrigin { max_hops: ONE_HOP });
    if case.explicit {
        request = request.header(TRACEPARENT, EXPLICIT_TRACEPARENT)?;
    }
    Ok(match case.policy {
        Policy::Default => request,
        Policy::Enabled => request.trace_context(true),
        Policy::Disabled => request.trace_context(false),
    })
}

/// The row label of a body read on a task of its own.
const READ_ELSEWHERE: &str = "a body read on another task";

/// Read a streamed body to its clean end on a task of its own.
///
/// The body handle moves to that task. The operation's trace context was
/// taken at the first poll of the send, so the move changes nothing a peer
/// receives.
async fn read_elsewhere(mut body: ClientBody) -> Result<(), RuntimeError> {
    let reading: tokio::task::JoinHandle<Result<(), RuntimeError>> = tokio::spawn(async move {
        let (_, end) = read_out(READ_ELSEWHERE, &mut body).await;
        end.map(drop)
    });
    reading
        .await
        .unwrap_or_else(|joined| panic!("the reading task did not complete: {joined}"))
}

/// Take a streamed head's status, and read its body to the end on another
/// task.
async fn read_streamed(head: StreamHead) -> Result<u16, RuntimeError> {
    let response: ClientStreamResponse = head?;
    let status: u16 = response.status();
    read_elsewhere(response.into_body()).await?;
    Ok(status)
}

/// The terminal a trace row sends through.
#[derive(Clone, Copy, Debug)]
enum Terminal {
    /// `send`: the status is the collected answer's.
    Buffered,
    /// `send_streaming`: the status is the head's, and the body is read to
    /// its end on another task.
    Streamed,
}

impl Terminal {
    /// Call this terminal on `request` now. The returned future runs the
    /// operation to its final status.
    ///
    /// The terminal is called here and not at the first poll, so a row can
    /// park what it returns unpolled.
    fn start<'a>(
        self,
        request: ClientRequest<'a>,
    ) -> impl Future<Output = Result<u16, RuntimeError>> + Send + 'a {
        match self {
            Self::Buffered => {
                let send = request.send();
                Either::Left(async move { send.await.map(|answer| answer.status()) })
            }
            Self::Streamed => {
                let send = request.send_streaming();
                Either::Right(async move { read_streamed(send.await).await })
            }
        }
    }

    /// Send row `index` through this terminal, to its final status.
    async fn run_case(
        self,
        client: &Client,
        base: &str,
        index: usize,
        case: Case,
    ) -> Result<u16, RuntimeError> {
        self.start(case_request(client, base, index, case)?).await
    }

    /// Whether this terminal's rows include the three sibling rows.
    fn has_siblings(self) -> bool {
        matches!(self, Self::Buffered)
    }
}

/// Send the three sibling rows at once through one client.
async fn send_siblings(client: &Client, base: &str) -> Result<(), RuntimeError> {
    let (first, second, third) = tokio::join!(
        Terminal::Buffered.run_case(client, base, SIBLING_INDEX, SIBLINGS[0]),
        Terminal::Buffered.run_case(client, base, SIBLING_INDEX + 1, SIBLINGS[1]),
        Terminal::Buffered.run_case(client, base, SIBLING_INDEX + 2, SIBLINGS[2]),
    );
    first.and(second).and(third).map(drop)
}

/// Answer a `hop` target with a redirect to its `landed` sibling, and every
/// other target with a complete answer.
fn trace_respond(request: &Recorded) -> Vec<u8> {
    match request.target().strip_suffix("hop") {
        Some(prefix) => redirect(&format!("{prefix}landed")),
        None => answer("200 OK", &[], b"ok"),
    }
}

/// The trace a `traceparent` value belongs to.
fn trace_of(traceparent: &str) -> Option<&str> {
    traceparent.split('-').nth(1)
}

/// Require every request the peer read for row `index` to carry what the
/// row's policy sends under `ambient`.
///
/// A redirected row is two requests. The context is taken once, when the
/// operation starts, so both carry the same value.
fn assert_case(recording: &Recording, index: usize, case: Case, ambient: Option<&str>) {
    let row = format!("row {index}, {case:?}, ambient trace {ambient:?}");
    let prefix = Case::prefix(index);
    let requests: Vec<&Recorded> = recording
        .requests
        .iter()
        .filter(|request| request.target().starts_with(&prefix))
        .collect();
    assert_eq!(
        requests.len(),
        case.sends(),
        "{row}: the peer read {requests:?}"
    );
    for request in &requests {
        assert_eq!(
            *request.header_values(APPLICATION_HEADER),
            [APPLICATION_VALUE],
            "{row}: the application header on the wire"
        );
        let sent = request.header_values(TRACEPARENT);
        match case.sent(ambient) {
            Sent::Nothing => assert!(sent.is_empty(), "{row}: the peer received {sent:?}"),
            Sent::Explicit => assert_eq!(
                *sent,
                [EXPLICIT_TRACEPARENT],
                "{row}: the application's own value must arrive once and unchanged"
            ),
            Sent::Trace(trace) => {
                assert_eq!(sent.len(), 1, "{row}: the peer received {sent:?}");
                assert_eq!(
                    trace_of(sent[0]),
                    Some(trace),
                    "{row}: the peer received {sent:?}"
                );
            }
        }
    }
    let first = requests[0].header_values(TRACEPARENT);
    assert!(
        requests
            .iter()
            .all(|request| request.header_values(TRACEPARENT) == first),
        "{row}: the hops of one operation carried different contexts: {requests:?}"
    );
}

/// Require every row of the policy matrix, each under the trace its caller
/// held.
fn assert_matrix_rows(recording: &Recording) {
    for (index, case) in cases().enumerate() {
        let ambient = AMBIENT_TRACE.map(|trace| trace(index));
        assert_case(recording, index, case, ambient.as_deref());
    }
}

/// Require the sibling rows, each under the one trace their caller held.
fn assert_sibling_rows(recording: &Recording) {
    let shared = AMBIENT_TRACE.map(|trace| trace(SIBLING_INDEX));
    for (offset, case) in SIBLINGS.into_iter().enumerate() {
        assert_case(recording, SIBLING_INDEX + offset, case, shared.as_deref());
    }
}

/// The trace one policy row's caller holds, by the row's index: none without
/// `otel`.
#[cfg(not(feature = "otel"))]
const AMBIENT_TRACE: Option<fn(usize) -> String> = None;

/// The trace one policy row's caller holds, by the row's index: its inbound
/// request's.
#[cfg(feature = "otel")]
const AMBIENT_TRACE: Option<fn(usize) -> String> = Some(traced::inbound_trace);

/// Name row `index` and its failure, if its call failed.
#[cfg(not(feature = "otel"))]
fn row_failure<T>(index: usize, case: Case, result: Result<T, RuntimeError>) -> Option<String> {
    result
        .err()
        .map(|error| format!("row {index}, {case:?}: {error:?}"))
}

/// Name the sibling rows and their failure, if `terminal` has them and one
/// failed.
#[cfg(not(feature = "otel"))]
async fn sibling_failure(terminal: Terminal, client: &Client, base: &str) -> Option<String> {
    match terminal.has_siblings() {
        true => send_siblings(client, base)
            .await
            .err()
            .map(|error| format!("sibling rows: {error:?}")),
        false => None,
    }
}

/// Run every row of `terminal` from a task that holds no trace context.
///
/// Without `otel` there is no context to hold, so a bare Tokio runtime is the
/// whole fixture. Returns each row that failed.
#[cfg(not(feature = "otel"))]
fn every_row(base: &str, terminal: Terminal) -> Vec<String> {
    common::block_on_detached(async {
        let client = http::client();
        let mut failures = Vec::new();
        for (index, case) in cases().enumerate() {
            let ended = terminal.run_case(&client, base, index, case).await;
            failures.extend(row_failure(index, case, ended));
        }
        failures.extend(sibling_failure(terminal, &client, base).await);
        failures
    })
}

#[cfg(feature = "otel")]
mod traced {
    //! The rows that need a trace context: each call runs inside a served
    //! handler, behind the tracing middleware, under the trace its inbound
    //! request named.

    use super::{
        APPLICATION_HEADER, APPLICATION_VALUE, Case, Policy, SIBLING_INDEX, TRACEPARENT, Terminal,
        assert_case, cases, send_siblings,
    };
    use crate::common;
    use crate::retry_upstream::Recording;

    use camber::http::Client;
    use camber::http::{self, Request, Response, Router, otel};
    use camber::{RuntimeError, runtime};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{Arc, LazyLock, Mutex, PoisonError};

    /// The one client every traced handler sends through.
    ///
    /// Static, so a send built in one handler can be parked and polled in
    /// another.
    static CLIENT: LazyLock<Client> = LazyLock::new(http::client);

    /// The index of the entry-timing row's target.
    const ENTRY_INDEX: usize = 200;

    /// The trace of the request whose handler builds the entry-timing send,
    /// and the trace of the request whose handler polls it.
    const BUILD_TRACE: &str = "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0";
    const POLL_TRACE: &str = "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1";

    const BUILD_ROUTE: &str = "/entry/build";
    const POLL_ROUTE: &str = "/entry/poll";
    const SIBLING_ROUTE: &str = "/siblings";

    /// The entry-timing row as the policy matrix would state it.
    const ENTRY_CASE: Case = Case {
        policy: Policy::Enabled,
        explicit: false,
        redirected: false,
    };

    /// An operation whose terminal was called and whose future was not yet
    /// polled. It ends at the operation's final status.
    type PendingSend = Pin<Box<dyn Future<Output = Result<u16, RuntimeError>> + Send>>;

    /// The trace the inbound request of row `index` names.
    pub(super) fn inbound_trace(index: usize) -> String {
        format!("{:032x}", 0xa000 + index)
    }

    /// The route whose handler sends row `index`.
    fn case_route(index: usize) -> String {
        format!("/case/{index}")
    }

    /// One route per row of `terminal`, the sibling route if it has sibling
    /// rows, and the two entry-timing routes, behind the tracing middleware.
    fn traced_router(base: &str, terminal: Terminal) -> Router {
        let base: Arc<str> = base.into();
        let mut router = Router::new();
        router.use_middleware(otel::tracing());
        for (index, case) in cases().enumerate() {
            let base = Arc::clone(&base);
            router.get(&case_route(index), move |_req: &Request| {
                let base = Arc::clone(&base);
                async move {
                    let status = terminal.run_case(&CLIENT, &base, index, case).await?;
                    Response::empty(status)
                }
            });
        }
        if terminal.has_siblings() {
            sibling_route(&mut router, &base);
        }
        entry_routes(&mut router, &base, terminal);
        router
    }

    /// The route whose handler sends the three sibling rows at once.
    fn sibling_route(router: &mut Router, base: &Arc<str>) {
        let shared = Arc::clone(base);
        router.get(SIBLING_ROUTE, move |_req: &Request| {
            let base = Arc::clone(&shared);
            async move {
                send_siblings(&CLIENT, &base).await?;
                Response::empty(200)
            }
        });
    }

    /// The two halves of the entry-timing row.
    ///
    /// The build route describes the request and calls `terminal` on it under
    /// its own trace, then parks the unpolled future. The poll route awaits
    /// that future under another trace. The operation starts at its first
    /// poll, so the peer must receive the poll route's trace. A streamed body
    /// is read on another task.
    fn entry_routes(router: &mut Router, base: &Arc<str>, terminal: Terminal) {
        let parked: Arc<Mutex<Option<PendingSend>>> = Arc::default();

        let built = Arc::clone(&parked);
        let url: Arc<str> = ENTRY_CASE.url(base, ENTRY_INDEX).into();
        router.get(BUILD_ROUTE, move |_req: &Request| {
            let built = Arc::clone(&built);
            let url = Arc::clone(&url);
            async move {
                let request = CLIENT
                    .request("GET", &url)?
                    .header(APPLICATION_HEADER, APPLICATION_VALUE)?
                    .trace_context(true);
                let pending: PendingSend = Box::pin(terminal.start(request));
                *built.lock().unwrap_or_else(PoisonError::into_inner) = Some(pending);
                Response::empty(200)
            }
        });

        router.get(POLL_ROUTE, move |_req: &Request| {
            let parked = Arc::clone(&parked);
            async move {
                let pending = parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                match pending {
                    Some(operation) => Response::empty(operation.await?),
                    None => Response::empty(409),
                }
            }
        });
    }

    /// Send one inbound request that names `trace`, and name the route if it
    /// was not answered 200.
    fn enter(addr: std::net::SocketAddr, route: &str, trace: &str) -> Option<String> {
        let inbound = format!("00-{trace}-b7ad6b7169203331-01");
        let answered = common::request(
            addr,
            "GET",
            route,
            &[(TRACEPARENT, inbound.as_str())],
            &[],
            common::WIRE_TIMEOUT,
        );
        match answered {
            Ok(response) if response.status == 200 => None,
            Ok(response) => Some(format!("{route}: answered {}", response.status)),
            Err(error) => Some(format!("{route}: never answered: {error}")),
        }
    }

    /// The sibling route and the trace its inbound request names, if
    /// `terminal` has sibling rows.
    fn sibling_entry(terminal: Terminal) -> Option<(String, String)> {
        terminal
            .has_siblings()
            .then(|| (SIBLING_ROUTE.to_owned(), inbound_trace(SIBLING_INDEX)))
    }

    /// The trace every row's inbound request names, by its route.
    ///
    /// The matrix rows come first, then `sibling` if the router serves it,
    /// then the two entry-timing routes.
    fn entries(sibling: Option<(String, String)>) -> Vec<(String, String)> {
        let matrix = (0..cases().count()).map(|index| (case_route(index), inbound_trace(index)));
        let entry = [
            (BUILD_ROUTE.to_owned(), BUILD_TRACE.to_owned()),
            (POLL_ROUTE.to_owned(), POLL_TRACE.to_owned()),
        ];
        matrix.chain(sibling).chain(entry).collect()
    }

    /// Serve `router`, enter every route of `entries` in order under its
    /// trace, and shut the runtime down.
    ///
    /// Returns each route that failed.
    fn enter_all(router: Router, entries: &[(String, String)]) -> Vec<String> {
        common::test_runtime()
            .run(|| {
                let addr = common::spawn_server(router);
                let failures: Vec<String> = entries
                    .iter()
                    .filter_map(|(route, trace)| enter(addr, route, trace))
                    .collect();
                runtime::request_shutdown();
                failures
            })
            .expect("the fixture runtime ran to completion")
    }

    /// Run every row of `terminal` from a served handler that holds its
    /// request's trace, then the entry-timing row.
    ///
    /// Returns each route that failed.
    pub(super) fn every_row(base: &str, terminal: Terminal) -> Vec<String> {
        let entered = entries(sibling_entry(terminal));
        enter_all(traced_router(base, terminal), &entered)
    }

    /// The entry-timing row: the peer received the trace of the request that
    /// polled the send, not the trace of the request that built it.
    pub(super) fn assert_configuration_rows(recording: &Recording) {
        assert_case(recording, ENTRY_INDEX, ENTRY_CASE, Some(POLL_TRACE));
    }
}

#[cfg(feature = "otel")]
use traced::every_row;

/// Run every trace row of `terminal` against one real peer, and require what
/// each of its requests carried.
fn assert_trace_rows(row: &str, terminal: Terminal) {
    let peers = peer_runtime();
    let upstream = peers.block_on(RecordingUpstream::bind_responder(trace_respond));

    let failures = every_row(&upstream.url(""), terminal);
    let recording = peers.block_on(upstream.finish(row));

    assert!(
        failures.is_empty(),
        "{row}: some rows were not answered: {failures:?}"
    );
    assert_matrix_rows(&recording);
    if terminal.has_siblings() {
        assert_sibling_rows(&recording);
    }
    // Without `otel` the policy matrix is the whole claim.
    #[cfg(feature = "otel")]
    traced::assert_configuration_rows(&recording);
}

/// 7.T2
///
/// One real peer records every request. With `otel`, each call runs in a
/// served handler under the trace its inbound request named. Without it, no
/// task holds a trace, and only an explicit header reaches the peer.
#[test]
fn outbound_trace_context_obeys_request_policy() {
    assert_trace_rows("trace policy rows", Terminal::Buffered);
}

// Streaming 5.T5: one streamed operation records one completion, after its
// source is released.
//
// The rows read the same `http.client.completed` event the buffered rows
// read, in a private child of their own. A streamed operation ends at its
// body's end, not at its head, so every row first requires an empty capture
// while it holds the head. Status conversion and source timing are proved in
// `acceptance_owned_lifecycle` (5.T4). This root owns the event.
//
// Every row that holds a head watches its peer's body owner. A probe reads
// the owner's `source_released` inside the recording of each completion, on
// the thread that records it. The reading does not depend on which task runs
// next, so it holds on the served runtime as on the current-thread one.
//
// | Clause | Row | Mutation |
// |---|---|---|
// | An unstarted call records nothing | `unstarted_streams_record_nothing` | record at description |
// | A refused start records one event and sends nothing | `refused_streams_record_zero_attempts`, `unexecuted_stream_is_recorded`, `closed_admission_is_recorded` | skip the record on an early return |
// | Success is recorded at the clean end only | `stream_success_is_recorded_at_the_end` | settle at the head |
// | A body failure keeps the head's status | `BODY_FAILURES` | overwrite the final status |
// | Drop, close, and conversion record one abandonment | `ABANDONMENTS` | emit again on close |
// | A committed cause is kept | `committed_cause_survives_conversion` | let conversion write a second terminal |
// | Attempts and redirects are the peer's count | `stream_hops_are_counted` | count from a constant |
// | A streamed request carries its body and headers | `streamed_request_sentinels_travel` | drop the body of a streamed request |
// | The source is released when the event is recorded | `assert_recorded_after_release`, in every row that holds a head | record, yield, then release |
// | Shutdown records `cancelled` | `served_stream_is_cancelled_at_shutdown` | record shutdown as abandonment |
// | The first poll's trace context is sent, wherever the body is read | `assert_streams_carry_the_first_poll_context` | lose the captured span |
// | No sentinel is recorded | `watch_for_secrets`, open for the whole child | leak one sentinel |

/// The header line every streaming peer's head carries.
const REPLY_HEADER: (&str, &str) = ("X-Reply", REPLY_HEADER_SECRET);

/// A bound a streaming row lets expire, measured from the first poll of its
/// send.
///
/// It is longer than the fixture watchdog, so real time cannot cross it while
/// a row reaches its head. The row then crosses it on a paused clock.
const STREAM_EXPIRY: Duration = Duration::from_secs(30);

/// The demand idle interval of the idle row. Real time crosses it: the
/// interval starts when the owner accepts the demand, and the row's one view
/// of the owner belongs to its capture, so it has no edge to pause behind.
const STREAM_IDLE: Duration = Duration::from_millis(60);

/// A chunked head with this status line and the reply header sentinel.
fn stream_head(status_line: &str) -> Pace {
    Pace::Send(paced_head(status_line, &[REPLY_HEADER, CHUNKED]))
}

/// One chunk that is the reply body sentinel.
fn secret_chunk() -> Pace {
    Pace::Send(paced_chunk(REPLY_BODY_SECRET.as_bytes()))
}

/// A head, one chunk, and a transport the peer then holds open.
fn held_stream_steps(status_line: &str) -> Vec<Pace> {
    vec![stream_head(status_line), secret_chunk(), Pace::UntilClosed]
}

/// Send one described GET through the streaming terminal.
async fn stream_described(client: &Client, url: &str) -> StreamHead {
    described(client, "GET", url).send_streaming().await
}

/// Yield until the capture holds `count` events.
///
/// The owner records from its own task, so a row that only requested release
/// waits here. It observes. It records nothing.
async fn recorded(row: &str, capture: &common::TraceCapture, count: usize) {
    if yielded_until(|| capture.len() >= count).await.is_none() {
        panic!(
            "{row}: the operation recorded {} of {count} completion(s)",
            capture.len()
        );
    }
}

/// Cross [`STREAM_EXPIRY`] behind a delivered head, and wait for the one
/// completion the expiry records.
///
/// The head arrived on real time, so the peer has nothing more to send. The
/// clock is paused only now, and the driver keeps it still. The bound started
/// at the first poll, so one whole bound from here is past it. Real time
/// resumes before the row waits on its peer again.
async fn expire_retained(row: &str, capture: &common::TraceCapture) {
    tokio::time::pause();
    let driver = RunnableDriver::start();
    tokio::time::advance(STREAM_EXPIRY + TIMER_ROUNDING).await;
    recorded(row, capture, 1).await;
    driver.stop().await;
    tokio::time::resume();
}

/// `error`, required safe in every rendering and source.
fn safe_error(row: &str, error: RuntimeError, addr: Option<SocketAddr>) -> RuntimeError {
    assert_safe(&error, &secrets(addr), row);
    error
}

/// Require a rendering that holds no sentinel.
fn assert_rendering_is_safe(row: &str, rendering: &str) {
    common::assert_private_absent(rendering, &SECRETS, "a Debug rendering", row);
}

/// Whether the watched owner had released its source, read inside the
/// recording of each completion.
type AtCompletion = Arc<Mutex<Vec<bool>>>;

/// Capture the completions recorded while the peer at `addr` is watched, and
/// read its body owner at each one.
///
/// The watcher must exist before the send, because a stream with no watcher
/// counts nothing. The capture owns the watcher, so both end together.
fn capture_at_release(addr: SocketAddr) -> (common::TraceCapture, AtCompletion) {
    let watched = watch_streams(addr);
    let released = AtCompletion::default();
    let probe = common::SourceReleaseProbe::new(watched, Arc::clone(&released));
    let capture = common::capture_events_probed(COMPLETED, probe);
    (capture, released)
}

/// Require one completion, recorded with the source already released.
fn assert_recorded_after_release(row: &str, released: &AtCompletion) {
    let readings = released.lock().unwrap_or_else(|error| error.into_inner());
    assert_eq!(
        readings.as_slice(),
        [true],
        "{row}: whether the source was released when each completion was recorded"
    );
}

/// Send `request` through the streaming terminal, and require its head.
async fn send_stream(row: &str, request: ClientRequest<'_>) -> ClientStreamResponse {
    let head: StreamHead = must_settle(row, "the send", request.send_streaming()).await;
    answered(row, head)
}

/// One streamed response whose head has arrived, its peer, its capture, and
/// the release readings of that capture.
struct Streamed {
    peer: PacedUpstream,
    addr: SocketAddr,
    capture: common::TraceCapture,
    released: AtCompletion,
    response: ClientStreamResponse,
}

/// Send one described GET at a peer that answers with `steps`, and require
/// its head and an empty capture.
///
/// The capture is the row's own. A head that recorded a completion would be
/// in it already.
async fn open_stream(
    row: &str,
    client: &Client,
    limit: Option<usize>,
    steps: Vec<Pace>,
) -> Streamed {
    let peer = PacedUpstream::bind_steps(steps).await;
    let addr = peer.addr();
    let url = secret_url(&peer.url(""), ANSWER);
    let (capture, released) = capture_at_release(addr);
    let request = match limit {
        Some(limit) => described(client, "GET", &url)
            .response_limit(limit)
            .unwrap_or_else(|error| panic!("{row}: the limit was refused: {error:?}")),
        None => described(client, "GET", &url),
    };
    let response: ClientStreamResponse = send_stream(row, request).await;
    settle_wakeups().await;
    assert!(
        capture.is_empty(),
        "{row}: a delivered head recorded a completion: {:?}",
        capture.events()
    );
    assert_rendering_is_safe(row, &format!("{response:?} {response:#?}"));
    Streamed {
        peer,
        addr,
        capture,
        released,
        response,
    }
}

/// Wait for the one completion, join the peer, and require the event to
/// record `expected`, the source to have been released when it was recorded,
/// and the peer to have read as many requests as the event counts attempts.
///
/// The row yields once more after the event arrived, so a second event the
/// owner still owed would be in the capture too.
async fn assert_stream_completed(
    row: &str,
    capture: &common::TraceCapture,
    released: &AtCompletion,
    peer: PacedUpstream,
    expected: &Completion,
) {
    recorded(row, capture, 1).await;
    settle_wakeups().await;
    let recording = peer.finish(row).await;
    assert_completion(row, capture, expected);
    assert_recorded_after_release(row, released);
    assert_peer_read(row, &recording, expected);
}

/// A streamed operation succeeds at its clean end. The head and a delivered
/// chunk record nothing, and a close behind the end records nothing more.
async fn stream_success_is_recorded_at_the_end() {
    let row = "a stream read to its end";
    let steps = vec![
        stream_head("200 OK"),
        secret_chunk(),
        Pace::Gate,
        Pace::Send(paced_end(&[])),
    ];
    let Streamed {
        peer,
        capture,
        released,
        response,
        ..
    } = open_stream(row, &http::client(), None, steps).await;

    let header: Option<Vec<u8>> = response.header("x-reply").map(<[u8]>::to_vec);
    let mut body: ClientBody = response.into_body();
    assert_rendering_is_safe(row, &format!("{body:?}"));
    let first: StreamNext = must_settle(row, "the first demand", body.next()).await;
    settle_wakeups().await;
    let after_chunk = capture.len();
    peer.open_gate();
    let end: StreamNext = must_settle(row, "the last demand", body.next()).await;
    let at_end = capture.len();
    let closed: Result<(), RuntimeError> = close(row, body).await;

    assert_eq!(
        header.as_deref(),
        Some(REPLY_HEADER_SECRET.as_bytes()),
        "{row}: the answer's header, so its absence elsewhere is a redaction"
    );
    assert!(
        matches!(&first, Ok(Some(chunk)) if &chunk[..] == REPLY_BODY_SECRET.as_bytes()),
        "{row}: the first demand returned {first:?}"
    );
    assert_eq!(
        after_chunk, 0,
        "{row}: a delivered chunk recorded a completion"
    );
    assert!(matches!(end, Ok(None)), "{row}: the body ended as {end:?}");
    assert_eq!(
        at_end, 1,
        "{row}: the end returned before the completion was recorded"
    );
    assert!(
        matches!(closed, Ok(())),
        "{row}: the close returned {closed:?}"
    );
    let expected = Completion::answered("GET", "200", 1);
    assert_stream_completed(row, &capture, &released, peer, &expected).await;
}

/// How one streamed body fails after its head.
#[derive(Clone, Copy, Debug)]
enum BodyFailure {
    /// A chunk crosses the request's maximum.
    TooLarge,
    /// The peer closes before the declared length arrived.
    Read,
    /// A demand waits past the idle interval.
    Idle,
    /// The retained response passes its attempt bound.
    Attempt,
    /// The retained response passes its operation total.
    Total,
}

const BODY_FAILURES: [BodyFailure; 5] = [
    BodyFailure::TooLarge,
    BodyFailure::Read,
    BodyFailure::Idle,
    BodyFailure::Attempt,
    BodyFailure::Total,
];

impl BodyFailure {
    fn steps(self) -> Vec<Pace> {
        match self {
            Self::TooLarge => vec![
                stream_head("200 OK"),
                secret_chunk(),
                Pace::Send(paced_chunk(b"x")),
                Pace::UntilClosed,
            ],
            Self::Read => vec![
                Pace::Send(paced_head(
                    "200 OK",
                    &[REPLY_HEADER, ("Content-Length", "64")],
                )),
                Pace::Send(REPLY_BODY_SECRET.as_bytes().to_vec()),
                Pace::Close,
            ],
            Self::Idle | Self::Attempt | Self::Total => {
                vec![stream_head("200 OK"), Pace::UntilClosed]
            }
        }
    }

    fn client(self) -> Client {
        let client = http::client()
            .total_timeout(UNREACHED)
            .request_timeout(UNREACHED)
            .response_idle_timeout(UNREACHED);
        match self {
            Self::TooLarge | Self::Read => client,
            Self::Idle => client.response_idle_timeout(STREAM_IDLE),
            Self::Attempt => client.request_timeout(STREAM_EXPIRY),
            Self::Total => client.total_timeout(STREAM_EXPIRY),
        }
    }

    fn limit(self) -> Option<usize> {
        match self {
            Self::TooLarge => Some(REPLY_BODY_SECRET.len()),
            Self::Read | Self::Idle | Self::Attempt | Self::Total => None,
        }
    }

    /// Whether the owner ends the retained response with no demand.
    fn expires_unread(self) -> bool {
        matches!(self, Self::Attempt | Self::Total)
    }

    fn category(self) -> ClientError {
        match self {
            Self::TooLarge => ClientError::ResponseTooLarge {
                limit: REPLY_BODY_SECRET.len(),
            },
            Self::Read => ClientError::Transport,
            Self::Idle => ClientError::Timeout(ClientTimeout::ResponseIdle),
            Self::Attempt => ClientError::Timeout(ClientTimeout::Attempt),
            Self::Total => ClientError::Timeout(ClientTimeout::Operation),
        }
    }

    fn outcome(self) -> &'static str {
        match self {
            Self::TooLarge => "response-too-large",
            Self::Read => "transport",
            Self::Idle | Self::Attempt | Self::Total => "timeout",
        }
    }
}

/// A failure after the head records one completion that keeps the head's
/// status. A retained response that expires records it with no demand.
async fn body_failure_keeps_the_final_status(case: BodyFailure) {
    let row = format!("a streamed body that fails: {case:?}");
    let row = row.as_str();
    let Streamed {
        peer,
        addr,
        capture,
        released,
        response,
    } = open_stream(row, &case.client(), case.limit(), case.steps()).await;

    if case.expires_unread() {
        expire_retained(row, &capture).await;
    }
    let mut body: ClientBody = response.into_body();
    let (_, end) = read_out(row, &mut body).await;
    let at_failure = capture.len();
    let closed: Result<(), RuntimeError> = close(row, body).await;

    assert!(
        failed_as(&end, case.category()),
        "{row}: the body ended as {end:?}"
    );
    assert_eq!(
        at_failure, 1,
        "{row}: the failure returned before the completion was recorded"
    );
    assert!(
        failed_as(&closed, case.category()),
        "{row}: the close returned {closed:?}"
    );
    for error in [end.err(), closed.err()].into_iter().flatten() {
        safe_error(row, error, Some(addr));
    }
    let expected = Completion::answered("GET", "200", 1).outcome(case.outcome());
    assert_stream_completed(row, &capture, &released, peer, &expected).await;
}

/// Convert a 500 response's status, and require the status error, safe in
/// every rendering.
fn assert_converted_to_status(row: &str, response: ClientStreamResponse, addr: SocketAddr) {
    let converted: Result<u16, RuntimeError> = response
        .error_for_status()
        .map(|kept: ClientStreamResponse| kept.status());
    match converted {
        Err(error) if matches!(error, RuntimeError::Client(ClientError::Status(500))) => {
            safe_error(row, error, Some(addr));
        }
        other => panic!("{row}: the conversion returned {other:?}"),
    }
}

/// How the caller gives up one live streamed response.
#[derive(Clone, Copy, Debug)]
enum Abandonment {
    /// Drop the unread response.
    Drop,
    /// Close the body after one chunk.
    Close,
    /// Convert the unread response's error status.
    Conversion,
}

const ABANDONMENTS: [Abandonment; 3] = [
    Abandonment::Drop,
    Abandonment::Close,
    Abandonment::Conversion,
];

/// Giving up a live response records one `abandoned` completion with the
/// head's status, once the owner has released its source.
///
/// A completed close acknowledges release, so its event is recorded at the
/// return. A drop and a conversion only request release, so the row waits.
async fn abandonment_is_recorded_once(case: Abandonment) {
    let row = format!("a live stream given up: {case:?}");
    let row = row.as_str();
    let (status_line, status) = match case {
        Abandonment::Drop | Abandonment::Close => ("200 OK", "200"),
        Abandonment::Conversion => ("500 Internal Server Error", "500"),
    };
    let Streamed {
        peer,
        addr,
        capture,
        released,
        response,
    } = open_stream(row, &http::client(), None, held_stream_steps(status_line)).await;

    match case {
        Abandonment::Drop => drop(response),
        Abandonment::Close => {
            let mut body: ClientBody = response.into_body();
            let first: StreamNext = demand(row, &mut body).await;
            let closed: Result<(), RuntimeError> = close(row, body).await;
            let at_close = capture.len();
            assert!(
                matches!(first, Ok(Some(_))),
                "{row}: the demand returned {first:?}"
            );
            assert!(
                matches!(closed, Ok(())),
                "{row}: the close returned {closed:?}"
            );
            assert_eq!(
                at_close, 1,
                "{row}: the close returned before the completion was recorded"
            );
        }
        Abandonment::Conversion => {
            assert_converted_to_status(row, response, addr);
        }
    }
    let expected = Completion::answered("GET", status, 1).outcome("abandoned");
    assert_stream_completed(row, &capture, &released, peer, &expected).await;
}

/// A conversion behind a committed failure records nothing more and changes
/// nothing: the one completion stays the timeout that ended the operation.
async fn committed_cause_survives_conversion() {
    let row = "a conversion after the operation total";
    let client: Client = http::client()
        .total_timeout(STREAM_EXPIRY)
        .request_timeout(UNREACHED);
    let Streamed {
        peer,
        addr,
        capture,
        released,
        response,
    } = open_stream(
        row,
        &client,
        None,
        held_stream_steps("500 Internal Server Error"),
    )
    .await;

    expire_retained(row, &capture).await;
    assert_converted_to_status(row, response, addr);
    let expected = Completion::answered("GET", "500", 1).outcome("timeout");
    assert_stream_completed(row, &capture, &released, peer, &expected).await;
}

/// Read a streamed body out, stop the peer, and require the reply's payload
/// and a clean end.
///
/// The body is borrowed, so the caller still holds it when it reads the
/// completion.
async fn read_to_clean_end(
    row: &str,
    body: &mut ClientBody,
    upstream: RecordingUpstream,
) -> Recording {
    let (payload, end) = read_out(row, body).await;
    let recording = upstream.finish(row).await;

    assert_eq!(
        payload,
        REPLY_BODY_SECRET.as_bytes(),
        "{row}: the delivered payload"
    );
    assert!(matches!(end, Ok(None)), "{row}: the body ended as {end:?}");
    recording
}

/// Read a streamed response to its clean end, stop the peer, and require one
/// completion that counts what the peer read. Return what the peer read.
///
/// The body is held until the completion is read.
async fn assert_stream_counted(
    row: &str,
    response: ClientStreamResponse,
    upstream: RecordingUpstream,
    capture: &common::TraceCapture,
    expected: &Completion,
) -> Recording {
    let mut body: ClientBody = response.into_body();
    let recording = read_to_clean_end(row, &mut body, upstream).await;

    assert_completion(row, capture, expected);
    assert_peer_read(row, &recording, expected);
    recording
}

/// Stream one described GET at the scripted recording peer, read it to its
/// end, and require one completion that counts what the peer read.
async fn stream_hops(row: &str, client: &Client, scripted: &str, expected: &Completion) {
    let upstream = RecordingUpstream::bind_responder(respond).await;
    let url = secret_url(&upstream.url(""), scripted);
    let capture = common::capture_events(COMPLETED);
    let request = described(client, "GET", &url)
        .redirect_policy(RedirectPolicy::SameOrigin { max_hops: TWO_HOPS });

    let response = send_stream(row, request).await;
    assert_stream_counted(row, response, upstream, &capture, expected).await;
}

/// The attempt and redirect counts of a streamed operation are the sends the
/// peer read before the final head.
async fn stream_hops_are_counted() {
    stream_hops(
        "a stream after one followed redirect",
        &http::client(),
        FOLLOWED,
        &Completion::answered("GET", "200", 2).redirects(1),
    )
    .await;
    stream_hops(
        "a stream after one retry",
        &retrying_client(),
        FLAKY,
        &Completion::answered("GET", "200", 2),
    )
    .await;
}

/// A streamed request sends its body and its headers, and its one completion
/// names its method.
///
/// The peer must read the body sentinel and both header sentinels. Their
/// absence from the whole-child captures is then a redaction, and not a
/// request that never carried them.
async fn streamed_request_sentinels_travel() {
    let row = "a streamed request with a body";
    let upstream = RecordingUpstream::bind_responder(respond).await;
    let url = secret_url(&upstream.url(""), ANSWER);
    let capture = common::capture_events(COMPLETED);
    let client = http::client();
    let request = described(&client, "POST", &url).body(BODY_SECRET);
    assert_rendering_is_safe(row, &format!("{request:?} {request:#?}"));

    let response = send_stream(row, request).await;
    assert_rendering_is_safe(row, &format!("{response:?} {response:#?}"));
    let expected = Completion::answered("POST", "200", 1);
    let recording = assert_stream_counted(row, response, upstream, &capture, &expected).await;

    assert_request_sentinels(row, &recording.requests[0], "POST");
}

/// A stream its client's policy or budget refuses starts the operation and
/// sends nothing. Each records one completion with zero attempts.
async fn refused_streams_record_zero_attempts() {
    let refusals: [(&str, fn() -> Client, ClientError, &'static str); 2] = [
        (
            "a stream with no byte maximum",
            || http::client().unbounded_response(),
            ClientError::InvalidRequest(ClientInput::StreamBudget),
            "invalid-request",
        ),
        (
            "a stream to a restricted literal",
            || http::client().egress_policy(guarded_policy()),
            ClientError::Egress(ClientEgress::Address),
            "egress",
        ),
    ];
    for (row, client, category, outcome) in refusals {
        let upstream = RecordingUpstream::bind_responder(respond).await;
        let addr = upstream.addr();
        let url = secret_url(&upstream.url(""), ANSWER);
        let capture = common::capture_events(COMPLETED);

        let head: StreamHead =
            must_settle(row, "the send", stream_described(&client(), &url)).await;
        let recording = upstream.finish_probed(row).await;

        assert_safe_refusal(row, head.map(drop), category, Some(addr));
        assert_completion(row, &capture, &Completion::unanswered("GET", 0, outcome));
        assert_eq!(
            recording.connections, 0,
            "{row}: a refused stream dialed its target"
        );
    }
}

/// A builder refusal and an unpolled streaming send start no operation, so
/// neither records an event or dials.
async fn unstarted_streams_record_nothing() {
    let row = "unstarted streams";
    let upstream = RecordingUpstream::bind_responder(respond).await;
    let addr = upstream.addr();
    let url = secret_url(&upstream.url(""), ANSWER);
    let userinfo = secret_url(
        &format!("http://{LOCATION_USER_SECRET}:{LOCATION_PASSWORD_SECRET}@{addr}"),
        ANSWER,
    );
    let capture = common::capture_events(COMPLETED);
    let client = http::client();

    let limited = described(&client, "GET", &url).response_limit(0).map(drop);
    let named = client.request("GET", &userinfo).map(drop);
    drop(described(&client, "GET", &url).send_streaming());
    settle_wakeups().await;

    let events = capture.events();
    let recording = upstream.finish_probed(row).await;

    assert_safe_refusal(
        "a zero response limit on a stream",
        limited,
        ClientError::InvalidRequest(ClientInput::ResponseLimit),
        None,
    );
    assert_safe_refusal(
        "a URL with userinfo",
        named,
        ClientError::InvalidRequest(ClientInput::Url),
        Some(addr),
    );
    assert!(
        events.is_empty(),
        "{row}: a stream that never started recorded {events:?}"
    );
    assert_eq!(
        recording.connections, 0,
        "{row}: a stream that never started dialed its target"
    );
}

/// A streaming send polled with no executor starts the operation and ends it
/// at once, with nothing dialed.
fn unexecuted_stream_is_recorded() {
    assert_unexecuted("a streaming send polled with no executor", |client, url| {
        stream_described(client, url)
            .now_or_never()
            .map(|head: StreamHead| head.map(|response| response.status()))
    });
}

/// A streaming send its runtime refuses to admit records one completion and
/// dials nothing.
fn closed_admission_is_recorded() {
    let row = "a streaming send after admission closed";
    assert_refused_undialed(row, "transport", |url, addr| {
        let refused: Result<u16, RuntimeError> = common::test_runtime()
            .run(|| {
                runtime::request_shutdown();
                let client = http::client();
                let head: StreamHead = runtime::block_on(stream_described(&client, url));
                head.map(|response: ClientStreamResponse| response.status())
            })
            .expect("the fixture runtime ran to completion");

        let error = match refused {
            Err(error) => safe_error(row, error, Some(addr)),
            Ok(status) => panic!("{row}: the refused stream answered {status}"),
        };
        assert!(
            matches!(error, RuntimeError::ScopeClosed),
            "{row}: returned {error:?}"
        );
    });
}

/// The route whose handler streams from the peer and waits on its body.
const STREAM_ROUTE: &str = "/outbound-stream-held";

/// Stream from the peer, report the head's status, and place one demand on
/// the body. The body is dropped when the demand returns.
async fn hold_stream(client: &Client, url: &str, heads: &Sender<u16>) -> Result<u16, RuntimeError> {
    let response: ClientStreamResponse = stream_described(client, url).await?;
    let status: u16 = response.status();
    // A closed receiver means the row has already finished.
    let _ = heads.send(status);
    let mut body: ClientBody = response.into_body();
    let next: StreamNext = body.next().await;
    next.map(|_| status)
}

/// A router whose one route streams from the peer, reports the head's
/// status, places a demand the peer never answers, and reports how it ended.
fn held_stream_router(
    url: String,
    heads: Sender<u16>,
    ends: Sender<Result<u16, RuntimeError>>,
) -> Router {
    let url: Arc<str> = url.into();
    let mut router = Router::new();
    router.get(STREAM_ROUTE, move |_req: &Request| {
        let (url, heads, ends) = (Arc::clone(&url), heads.clone(), ends.clone());
        async move {
            let client = http::client().total_timeout(UNREACHED);
            let ended: Result<u16, RuntimeError> = hold_stream(&client, &url, &heads).await;
            let _ = ends.send(ended);
            Response::empty(200)
        }
    });
    router
}

/// A served handler's stream, held at a demand, ends with the runtime's
/// shutdown and records one `cancelled` completion with the head's status,
/// once the owner has released its source.
///
/// The peer runs on a runtime of its own. The inbound peer stays open until
/// the handler has reported, so its close cannot be what ended the stream.
fn served_stream_is_cancelled_at_shutdown() {
    let row = "a handler's stream, held at shutdown";
    let peers = peer_runtime();
    let mut upstream = on_peers(
        &peers,
        PacedUpstream::bind_steps(vec![stream_head("200 OK"), Pace::UntilClosed]),
    );
    let (heads, headed) = channel();
    let (ends, ended) = channel();
    let router = held_stream_router(secret_url(&upstream.url(""), ANSWER), heads, ends);
    let (capture, at_completion) = capture_at_release(upstream.addr());

    let (head, early, end, released) = common::test_runtime()
        .run(|| {
            let addr = common::spawn_server(router);
            let inbound = std::thread::spawn(move || {
                common::request(addr, "GET", STREAM_ROUTE, &[], &[], common::WIRE_TIMEOUT)
            });
            let head = headed.recv_timeout(WATCHDOG).ok();
            let early = capture.len();
            runtime::request_shutdown();
            let end = ended.recv_timeout(WATCHDOG).ok();
            let released = on_peers(&peers, upstream.client_left());
            // The inbound answer is not this row's evidence: shutdown may
            // close the connection before the handler's answer is written.
            let _ = inbound.join();
            (head, early, end, released)
        })
        .expect("the fixture runtime ran to completion");
    let recording = on_peers(&peers, upstream.finish(row));

    assert_eq!(head, Some(200), "{row}: the head the handler was given");
    assert_eq!(early, 0, "{row}: a delivered head recorded a completion");
    assert!(
        matches!(end, Some(Err(RuntimeError::Cancelled))),
        "{row}: the handler's demand returned {end:?}"
    );
    if let Err(open) = released {
        panic!("{row}: {open}");
    }
    assert_completion(
        row,
        &capture,
        &Completion::answered("GET", "200", 1).outcome("cancelled"),
    );
    assert_recorded_after_release(row, &at_completion);
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// Every streaming row that needs no Camber runtime, on one bare Tokio
/// runtime.
fn bare_stream_rows() {
    common::block_on_detached(async {
        unstarted_streams_record_nothing().await;
        refused_streams_record_zero_attempts().await;
        stream_success_is_recorded_at_the_end().await;
        stream_hops_are_counted().await;
        streamed_request_sentinels_travel().await;
        for case in BODY_FAILURES {
            body_failure_keeps_the_final_status(case).await;
        }
        for case in ABANDONMENTS {
            abandonment_is_recorded_once(case).await;
        }
        committed_cause_survives_conversion().await;
    });
}

/// A streamed operation sends the trace context its first poll held, under
/// every request policy, and reads its body on another task.
///
/// One real peer records every request. With `otel`, each call runs in a
/// served handler under the trace its inbound request named, and the
/// entry-timing row polls a send another request built. Without it, no task
/// holds a trace, and only an explicit header reaches the peer.
fn assert_streams_carry_the_first_poll_context() {
    assert_trace_rows("streamed trace policy rows", Terminal::Streamed);
}

/// Every 5.T5 row, in the private child.
fn assert_streaming_completions_follow_release() {
    let watched = watch_for_secrets();
    unexecuted_stream_is_recorded();
    bare_stream_rows();
    closed_admission_is_recorded();
    served_stream_is_cancelled_at_shutdown();
    assert_streams_carry_the_first_poll_context();
    assert_no_secret_was_recorded(&watched);
}

/// How long the private child may take to drive every streaming row.
///
/// Every row is bounded by the fixture watchdog, and none waits for it on a
/// healthy run. This bounds a child that hangs outside any row.
const STREAMING_CHILD_BOUND: Duration = Duration::from_secs(180);

/// Streaming 5.T5
#[test]
fn streaming_completion_follows_source_release() {
    common::run_in_child(
        "outbound_client::streaming_completion_follows_source_release",
        "outbound-streaming-completion-events",
        "OUTBOUND_STREAMING_COMPLETION_EVENTS_COMPLETE",
        STREAMING_CHILD_BOUND,
        assert_streaming_completions_follow_release,
    );
}
