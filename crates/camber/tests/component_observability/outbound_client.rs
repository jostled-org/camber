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
    Answer, Call, PeerEvent, Recorded, Recording, RecordingUpstream, RunnableDriver,
    ScriptedUpstream, TIMER_ROUNDING, WATCHDOG, answer, answered, assert_released, describe,
    failure, on_peers, peer_runtime, saw_each, settled, within_watchdog,
};
use crate::untrusted_tls::{TlsPeerEvent, untrusted_tls_peer};

use camber::http;
use camber::http::Client;
use camber::http::ClientDecode;
use camber::http::ClientError;
use camber::http::ClientInput;
use camber::http::ClientRedirect;
use camber::http::ClientRequest;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::RedirectPolicy;
use camber::http::mock;
use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
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

/// Every sentinel a request, an answer, or a redirect of this module carries.
const SECRETS: [&str; 11] = [
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
    let error = failure(row, result);
    assert_safe(&error, &secrets(Some(addr)), row);
    error
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
    match result {
        Some(result) => (result, capture),
        None => panic!("{row}: the call never settled"),
    }
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
        assert_eq!(
            self.recording.requests.len(),
            expected.attempts,
            "{row}: the peer read {:?}",
            self.recording.requests
        );
        assert_eq!(
            self.recording.starts as usize, expected.attempts,
            "{row}: the peer saw {} request starts",
            self.recording.starts
        );
    }
}

/// Run one call against a scripted peer of its own.
///
/// `call` is given the peer's base URL. The peer is stopped and joined before
/// anything is returned, so the recording is complete.
async fn drive<F, Fut>(row: &str, call: F) -> Seen
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<ClientResponse, RuntimeError>>,
{
    let upstream = RecordingUpstream::bind_responder(respond).await;
    let addr = upstream.addr();
    let capture = common::capture_events(COMPLETED);
    let result = within_watchdog(call(upstream.url(""))).await;
    let recording = upstream.finish(row).await;
    match result {
        Some(result) => Seen {
            result,
            capture,
            recording,
            addr,
        },
        None => panic!("{row}: the call never settled"),
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

/// Require the general row's request and answer to have carried the
/// sentinels, so their absence from every diagnostic is a redaction.
fn assert_sentinels_travelled(row: &str, seen: &Seen) {
    let sent = &seen.recording.requests[0];
    assert_eq!(sent.method(), "PURGE", "{row}: the method on the wire");
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
        &Completion {
            method: "GET",
            host: MOCK_HOST,
            status: "200",
            attempts: 0,
            redirects: 0,
            outcome: "response",
        },
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

/// A value whose serialization fails with a message that holds a sentinel.
struct Unencodable;

impl serde::Serialize for Unencodable {
    fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom(BODY_SECRET))
    }
}

/// Require a builder refusal with this category, safe in every rendering.
fn assert_builder_refusal(row: &str, refused: Result<(), RuntimeError>, expected: ClientError) {
    match refused {
        Err(error) => {
            assert!(
                matches!(error, RuntimeError::Client(category) if category == expected),
                "{row}: returned {error:?} instead of {expected:?}"
            );
            assert_safe(&error, &secrets(None), row);
        }
        Ok(()) => panic!("{row}: the builder accepted the description"),
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
    let recording = upstream.finish(row).await;

    assert_builder_refusal(
        "a refused method",
        method,
        ClientError::InvalidRequest(ClientInput::Method),
    );
    assert_builder_refusal(
        "a reserved header",
        reserved,
        ClientError::InvalidRequest(ClientInput::ReservedHeader),
    );
    assert_builder_refusal("an unencodable body", encoded, ClientError::Encode);
    assert_builder_refusal(
        "a zero response limit",
        limited,
        ClientError::InvalidRequest(ClientInput::ResponseLimit),
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

    let error = match result {
        Some(result) => safe_failure(row, result, addr),
        None => panic!("{row}: the call never settled"),
    };
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
/// The row runs before any runtime exists. The listener is never accepted on,
/// so an empty backlog proves the call dialed nothing.
fn unexecuted_call_is_recorded() {
    let row = "a call polled with no executor";
    let listener = common::UndialedListener::bind();
    let url = secret_url(&format!("http://{}", listener.addr()), ANSWER);
    let capture = common::capture_events(COMPLETED);
    let client = http::client();

    let polled = described(&client, "GET", &url).send().now_or_never();

    assert!(
        matches!(polled, Some(Err(RuntimeError::NoRuntime))),
        "{row}: returned {}",
        describe(&polled)
    );
    listener.assert_undialed(row);
    assert_completion(
        row,
        &capture,
        &Completion::unanswered("GET", 0, "no-runtime"),
    );
}

/// Every row that needs no Camber runtime, on one bare Tokio runtime.
fn bare_rows() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async {
        every_ingress_completes_once().await;
        mocked_call_completes_once().await;
        local_conversions_record_nothing().await;
        retries_are_counted_as_attempts().await;
        redirects_are_counted_and_refusals_recorded().await;
        crossed_limit_is_recorded().await;
        send_time_refusal_records_zero_attempts().await;
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

/// The route whose handler awaits a call the peer holds.
const HELD_ROUTE: &str = "/outbound-held";

/// The name the journey router's mapper records its refusals under.
const JOURNEY_MAPPER: &str = "outbound-journey";

/// A router with the two outbound routes, behind a mapper that records what
/// it was handed.
///
/// The refused route returns its call's failure, so the server maps it. The
/// held route reports what its call returned, so the row reads the result the
/// handler was given and not the answer the inbound peer got.
fn journey_router(
    held_url: String,
    ends: Sender<Result<u16, RuntimeError>>,
    journal: &common::Journal,
) -> Router {
    let mut router = Router::new();
    router.get(REFUSED_ROUTE, |_req: &Request| async {
        let client = http::client();
        let url = secret_url(
            &format!("http://{}", common::refused_loopback_addr()),
            ANSWER,
        );
        let answer = described(&client, "POST", &url)
            .body(BODY_SECRET)
            .send()
            .await?;
        Response::empty(answer.status())
    });

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
    /// Whether the peer confirmed the held call was parked at its head.
    reached: Result<(), String>,
    /// What the held call returned to its handler, or `None` if it never
    /// reported.
    end: Option<Result<u16, RuntimeError>>,
    /// Whether the client closed the held attempt's transport.
    released: Result<(), String>,
}

/// Drive the refused route, then hold a call and request shutdown.
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
        reached,
        end,
        released,
    }
}

/// Require the inbound peer and the mapper to have been given the fixed
/// internal refusal, with no sentinel in either.
fn assert_mapped_refusal(
    refused: std::io::Result<common::HttpResponse>,
    journal: &common::Journal,
) {
    let row = "a handler's refused call";
    let response = refused.unwrap_or_else(|error| panic!("{row}: never answered: {error}"));
    assert_eq!(response.status, 500, "{row}: wire status");
    assert_eq!(&*response.text(), common::REDACTED_BODY, "{row}: wire body");
    common::assert_no_private_text(&response, &SECRETS, row);
    common::assert_mapped_internal_refusal(journal, JOURNEY_MAPPER, &SECRETS, row);
}

/// The rows that need a Camber runtime: a served handler's refused call, and
/// a held call ended by runtime shutdown.
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

    assert_mapped_refusal(served.refused, &journal);
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
        2,
        "the two served calls record one completion each: {events:?}"
    );
    assert_event(
        "a handler's refused call",
        &events[0],
        &Completion::unanswered("POST", 1, "connect"),
    );
    assert_event(
        row,
        &events[1],
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

/// Send row `index` through the public request builder.
async fn send_case(
    client: &Client,
    base: &str,
    index: usize,
    case: Case,
) -> Result<ClientResponse, RuntimeError> {
    let mut request = client
        .request("GET", &case.url(base, index))?
        .header(APPLICATION_HEADER, APPLICATION_VALUE)?
        .redirect_policy(RedirectPolicy::SameOrigin { max_hops: ONE_HOP });
    if case.explicit {
        request = request.header(TRACEPARENT, EXPLICIT_TRACEPARENT)?;
    }
    let request = match case.policy {
        Policy::Default => request,
        Policy::Enabled => request.trace_context(true),
        Policy::Disabled => request.trace_context(false),
    };
    request.send().await
}

/// Send the three sibling rows at once through one client.
async fn send_siblings(client: &Client, base: &str) -> Result<(), RuntimeError> {
    let (first, second, third) = tokio::join!(
        send_case(client, base, SIBLING_INDEX, SIBLINGS[0]),
        send_case(client, base, SIBLING_INDEX + 1, SIBLINGS[1]),
        send_case(client, base, SIBLING_INDEX + 2, SIBLINGS[2]),
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

/// Require the policy matrix and the sibling rows, each under the trace its
/// caller held.
fn assert_policy_rows(recording: &Recording) {
    for (index, case) in cases().enumerate() {
        assert_case(recording, index, case, ambient_trace(index).as_deref());
    }
    let shared = ambient_trace(SIBLING_INDEX);
    for (offset, case) in SIBLINGS.into_iter().enumerate() {
        assert_case(recording, SIBLING_INDEX + offset, case, shared.as_deref());
    }
}

/// The context one policy row's caller holds: none without `otel`.
#[cfg(not(feature = "otel"))]
fn ambient_trace(_index: usize) -> Option<String> {
    None
}

/// Send every row from a task that holds no trace context.
///
/// Without `otel` there is no context to hold, so a bare Tokio runtime is the
/// whole fixture. Returns each row that failed.
#[cfg(not(feature = "otel"))]
fn send_every_row(base: &str) -> Vec<String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(async {
        let client = http::client();
        let mut failures = Vec::new();
        for (index, case) in cases().enumerate() {
            if let Err(error) = send_case(&client, base, index, case).await {
                failures.push(format!("row {index}, {case:?}: {error:?}"));
            }
        }
        if let Err(error) = send_siblings(&client, base).await {
            failures.push(format!("sibling rows: {error:?}"));
        }
        failures
    })
}

/// Without `otel` the policy matrix is the whole claim.
#[cfg(not(feature = "otel"))]
fn assert_configuration_rows(_recording: &Recording) {}

#[cfg(feature = "otel")]
mod traced {
    //! The rows that need a trace context: each call runs inside a served
    //! handler, behind the tracing middleware, under the trace its inbound
    //! request named.

    use super::{
        APPLICATION_HEADER, APPLICATION_VALUE, Case, Policy, SIBLING_INDEX, TRACEPARENT,
        assert_case, cases, send_case, send_siblings,
    };
    use crate::common;
    use crate::retry_upstream::Recording;

    use camber::http::Client;
    use camber::http::ClientResponse;
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

    /// A send that was built and not yet polled.
    type PendingSend = Pin<Box<dyn Future<Output = Result<ClientResponse, RuntimeError>> + Send>>;

    /// The trace the inbound request of row `index` names.
    fn inbound_trace(index: usize) -> String {
        format!("{:032x}", 0xa000 + index)
    }

    /// The context one policy row's caller holds: its inbound request's.
    pub(super) fn ambient_trace(index: usize) -> Option<String> {
        Some(inbound_trace(index))
    }

    /// The route whose handler sends row `index`.
    fn case_route(index: usize) -> String {
        format!("/case/{index}")
    }

    /// One route per row, the sibling route, and the two entry-timing routes,
    /// behind the tracing middleware.
    fn traced_router(base: &str) -> Router {
        let base: Arc<str> = base.into();
        let mut router = Router::new();
        router.use_middleware(otel::tracing());
        for (index, case) in cases().enumerate() {
            let base = Arc::clone(&base);
            router.get(&case_route(index), move |_req: &Request| {
                let base = Arc::clone(&base);
                async move {
                    let answer = send_case(&CLIENT, &base, index, case).await?;
                    Response::empty(answer.status())
                }
            });
        }
        let shared = Arc::clone(&base);
        router.get(SIBLING_ROUTE, move |_req: &Request| {
            let base = Arc::clone(&shared);
            async move {
                send_siblings(&CLIENT, &base).await?;
                Response::empty(200)
            }
        });
        entry_routes(&mut router, &base);
        router
    }

    /// The two halves of the entry-timing row.
    ///
    /// The build route describes the request and calls `send` under its own
    /// trace, then parks the unpolled future. The poll route awaits that
    /// future under another trace. The operation starts at its first poll, so
    /// the peer must receive the poll route's trace.
    fn entry_routes(router: &mut Router, base: &Arc<str>) {
        let parked: Arc<Mutex<Option<PendingSend>>> = Arc::default();

        let (built, url) = (Arc::clone(&parked), ENTRY_CASE.url(base, ENTRY_INDEX));
        router.get(BUILD_ROUTE, move |_req: &Request| {
            let built = Arc::clone(&built);
            let url = url.clone();
            async move {
                let request = CLIENT
                    .request("GET", &url)?
                    .header(APPLICATION_HEADER, APPLICATION_VALUE)?
                    .trace_context(true);
                let pending: PendingSend = Box::pin(request.send());
                *built.lock().unwrap_or_else(PoisonError::into_inner) = Some(pending);
                Response::empty(200)
            }
        });

        router.get(POLL_ROUTE, move |_req: &Request| {
            let parked = Arc::clone(&parked);
            async move {
                let pending = parked.lock().unwrap_or_else(PoisonError::into_inner).take();
                match pending {
                    Some(send) => Response::empty(send.await?.status()),
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

    /// The trace every row's inbound request names, by its route.
    fn entries() -> Vec<(String, String)> {
        let matrix = (0..cases().count()).map(|index| (case_route(index), inbound_trace(index)));
        let fixed = [
            (SIBLING_ROUTE, inbound_trace(SIBLING_INDEX)),
            (BUILD_ROUTE, BUILD_TRACE.to_owned()),
            (POLL_ROUTE, POLL_TRACE.to_owned()),
        ];
        matrix
            .chain(fixed.map(|(route, trace)| (route.to_owned(), trace)))
            .collect()
    }

    /// Send every row from a served handler that holds its request's trace.
    ///
    /// Returns each route that failed.
    pub(super) fn send_every_row(base: &str) -> Vec<String> {
        let router = traced_router(base);
        common::test_runtime()
            .run(|| {
                let addr = common::spawn_server(router);
                let failures: Vec<String> = entries()
                    .iter()
                    .filter_map(|(route, trace)| enter(addr, route, trace))
                    .collect();
                runtime::request_shutdown();
                failures
            })
            .expect("the fixture runtime ran to completion")
    }

    /// The entry-timing row: the peer received the trace of the request that
    /// polled the send, not the trace of the request that built it.
    pub(super) fn assert_configuration_rows(recording: &Recording) {
        assert_case(recording, ENTRY_INDEX, ENTRY_CASE, Some(POLL_TRACE));
    }
}

#[cfg(feature = "otel")]
use traced::{ambient_trace, assert_configuration_rows, send_every_row};

/// 7.T2
///
/// One real peer records every request. With `otel`, each call runs in a
/// served handler under the trace its inbound request named. Without it, no
/// task holds a trace, and only an explicit header reaches the peer.
#[test]
fn outbound_trace_context_obeys_request_policy() {
    let row = "trace policy rows";
    let peers = peer_runtime();
    let upstream = peers.block_on(RecordingUpstream::bind_responder(trace_respond));

    let failures = send_every_row(&upstream.url(""));
    let recording = peers.block_on(upstream.finish(row));

    assert!(
        failures.is_empty(),
        "{row}: some rows were not answered: {failures:?}"
    );
    assert_policy_rows(&recording);
    assert_configuration_rows(&recording);
}
