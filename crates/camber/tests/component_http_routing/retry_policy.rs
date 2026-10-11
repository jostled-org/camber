//! Outbound 5.T1 beside the retained disposal and generated-count rows: one
//! request's own permission decides every send after the first.
//!
//! The status rows read a recording peer, so each row states the count and the
//! exact bytes of every send. The transport rows read a scripted peer that
//! closes, stalls, or cuts each attempt, and the TLS row reads a peer no trust
//! store holds. No row reads elapsed time as evidence.

use crate::bounded_client::within_bound;
use crate::retry_upstream::{
    Answer, CLIENT_METHODS, Call, PeerEvent, Recorded, Recording, RecordingUpstream,
    RunnableDriver, ScriptedUpstream, TIMER_ROUNDING, answer, answered, assert_released, describe,
    payload, send_free_method, send_method, settle_wakeups, settled, within_watchdog,
};
use crate::untrusted_tls::{TlsPeerEvent, untrusted_tls_peer};

use camber::RuntimeError;
use camber::http;
use camber::http::Client;
use camber::http::ClientError;
use camber::http::ClientRequest;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::RetryPolicy;
use camber::http::{Request, Response, Router};
use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// Every method token a row sends: the idempotent methods first, then the
/// methods the default permission never repeats.
///
/// `PURGE` and `get` are extension tokens. A method token is case-sensitive,
/// so `get` is not GET and earns nothing GET is given.
pub(crate) const METHODS: [&str; 9] = [
    "GET", "HEAD", "OPTIONS", "PUT", "DELETE", "POST", "PATCH", "PURGE", "get",
];

/// How many leading [`METHODS`] are idempotent.
const IDEMPOTENT_METHOD_COUNT: usize = 5;

/// The replay permission one row gives its request.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Permission {
    /// The request sets none, so the documented default applies.
    Defaulted,
    Never,
    Idempotent,
    Replayable,
}

/// Every permission a request can run under.
pub(crate) const PERMISSIONS: [Permission; 4] = [
    Permission::Defaulted,
    Permission::Never,
    Permission::Idempotent,
    Permission::Replayable,
];

impl Permission {
    /// The policy the row writes on its request, or `None` to write nothing.
    pub(crate) fn policy(self) -> Option<RetryPolicy> {
        match self {
            Self::Defaulted => None,
            Self::Never => Some(RetryPolicy::Never),
            Self::Idempotent => Some(RetryPolicy::Idempotent),
            Self::Replayable => Some(RetryPolicy::Replayable),
        }
    }

    /// Whether this permission lets `method` be sent again after a transient
    /// outcome.
    pub(crate) fn permits(self, method: &str) -> bool {
        match self {
            Self::Never => false,
            Self::Defaulted | Self::Idempotent => {
                METHODS[..IDEMPOTENT_METHOD_COUNT].contains(&method)
            }
            Self::Replayable => true,
        }
    }
}

/// One described request: its method, its permission, and its own retry count.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Draft {
    pub(crate) method: &'static str,
    pub(crate) permission: Permission,
    /// The count written on the request, or `None` to keep the client's.
    pub(crate) retries: Option<u32>,
}

impl Draft {
    pub(crate) const fn new(method: &'static str, permission: Permission) -> Self {
        Self {
            method,
            permission,
            retries: None,
        }
    }

    /// The same request with its own retry count.
    pub(crate) const fn counted(self, retries: u32) -> Self {
        Self {
            retries: Some(retries),
            ..self
        }
    }
}

/// Describe `draft` through `Client::request`, and send nothing.
///
/// The permission and the count are written on the request and nowhere else.
pub(crate) fn describe_draft<'c>(
    client: &'c Client,
    draft: Draft,
    url: &str,
) -> Result<ClientRequest<'c>, RuntimeError> {
    let described = client.request(draft.method, url)?;
    let described = match payload(draft.method) {
        "" => described,
        body => described.body(body),
    };
    let described = match draft.permission.policy() {
        Some(policy) => described.retry_policy(policy),
        None => described,
    };
    Ok(match draft.retries {
        Some(count) => described.retries(count),
        None => described,
    })
}

/// Describe `draft` through `Client::request` and send it.
pub(crate) async fn send_draft(
    client: &Client,
    draft: Draft,
    url: &str,
) -> Result<ClientResponse, RuntimeError> {
    describe_draft(client, draft, url)?.send().await
}

/// Start one described request as its own task.
pub(crate) fn spawn_draft(client: Client, draft: Draft, url: String) -> Call {
    tokio::spawn(async move { send_draft(&client, draft, &url).await })
}

/// The statuses that can earn a permitted request another send.
const TRANSIENT_STATUSES: [u16; 4] = [429, 502, 503, 504];

/// Statuses no permission repeats: the answer itself, the refusals the server
/// already decided, and the server faults that name no retry.
const SETTLED_STATUSES: [u16; 7] = [200, 400, 404, 409, 500, 501, 505];

/// The retry counts every status row runs under: none, one, and several.
const RETRY_COUNTS: [u32; 3] = [0, 1, 3];

/// The base backoff of the rows that run on real time.
const BACKOFF: Duration = Duration::from_millis(1);

/// A complete answer with no body, which every method may receive.
fn status_reply(status: u16) -> Vec<u8> {
    answer(&format!("{status} Scripted"), &[], b"")
}

/// How many sends one request makes: one, plus its whole count when the
/// permission and the outcome both allow a repeat.
fn sends(draft: Draft, transient: bool, retries: u32) -> u32 {
    match draft.permission.permits(draft.method) && transient {
        true => retries + 1,
        false => 1,
    }
}

/// One status row: a request to its own target on the recording peer.
#[derive(Clone, Copy, Debug)]
struct StatusRow {
    status: u16,
    draft: Draft,
    /// The count the row's client was configured with.
    retries: u32,
}

impl StatusRow {
    /// The target only this row requests, so its sends are read by target.
    fn target(self) -> String {
        format!(
            "/{}/{:?}/{}/{}",
            self.status, self.draft.permission, self.retries, self.draft.method
        )
    }

    fn sends(self) -> u32 {
        sends(
            self.draft,
            TRANSIENT_STATUSES.contains(&self.status),
            self.retries,
        )
    }

    /// Send the row's request through `client`.
    ///
    /// A request that sets no permission goes through the verb convenience
    /// when the client offers one, so the default is proved at that ingress
    /// too.
    async fn send(self, client: &Client, url: &str) -> Result<ClientResponse, RuntimeError> {
        match (
            self.draft.permission,
            CLIENT_METHODS.contains(&self.draft.method),
        ) {
            (Permission::Defaulted, true) => send_method(client, self.draft.method, url).await,
            _ => send_draft(client, self.draft, url).await,
        }
    }
}

/// Every permission and method under one status and one client count.
fn status_rows(status: u16, retries: u32) -> impl Iterator<Item = StatusRow> {
    PERMISSIONS.into_iter().flat_map(move |permission| {
        METHODS.into_iter().map(move |method| StatusRow {
            status,
            draft: Draft::new(method, permission),
            retries,
        })
    })
}

/// Require every send of one request to carry its method and body, and every
/// repeat to be the first send byte for byte.
fn assert_identical_sends(sent: &[&Recorded], method: &str, context: &str) {
    let Some(first) = sent.first() else {
        panic!("{context}: the peer read no request");
    };
    for (index, request) in sent.iter().enumerate() {
        assert_eq!(request.method(), method, "{context}: send {index} method");
        assert_eq!(
            &*request.body,
            payload(method).as_bytes(),
            "{context}: send {index} body"
        );
        assert_eq!(
            request.head, first.head,
            "{context}: send {index} changed the request head"
        );
    }
}

/// Require one status row's answer, its send count, and its exact sends.
fn assert_status_row(
    row: StatusRow,
    result: Result<ClientResponse, RuntimeError>,
    recording: &Recording,
) {
    let context = format!("{row:?}");
    let sent = recording.sent_to(&row.target());
    let status = answered(&context, result).status();

    assert_eq!(status, row.status, "{context}: answered status");
    assert_eq!(
        sent.len(),
        row.sends() as usize,
        "{context}: the peer read {} sends",
        sent.len()
    );
    assert_identical_sends(&sent, row.draft.method, &context);
}

/// The target only the free verb of `method` requests.
fn free_target(status: u16, method: &str) -> String {
    format!("/{status}/free/{method}")
}

/// Send every free verb at the peer once, and report how many sends that is.
///
/// A free verb has no configured retries, so one call is one send whatever
/// the peer answers.
async fn send_free_verbs(status: u16, upstream: &RecordingUpstream, context: &str) -> usize {
    for method in CLIENT_METHODS {
        let url = upstream.url(&free_target(status, method));
        let answered: ClientResponse = within_bound(context, send_free_method(method, &url))
            .await
            .unwrap_or_else(|error| panic!("{context}: free {method} failed: {error:?}"));
        assert_eq!(answered.status(), status, "{context}: free {method} status");
    }
    CLIENT_METHODS.len()
}

/// Require each free verb to have been sent exactly once.
fn assert_free_verbs_sent_once(status: u16, recording: &Recording, context: &str) {
    for method in CLIENT_METHODS {
        let sent = recording.sent_to(&free_target(status, method));
        assert_eq!(
            sent.len(),
            1,
            "{context}: free {method} was sent {} times",
            sent.len()
        );
    }
}

/// One status against every permission, method, and count.
///
/// `clients` holds one client per entry of [`RETRY_COUNTS`]. Each row requests
/// its own target, so the recording states each row's sends apart from every
/// other row's.
async fn assert_status_rows(status: u16, clients: &[Client; RETRY_COUNTS.len()]) {
    let context = format!("status {status}");
    let upstream = RecordingUpstream::bind(status_reply(status)).await;
    let mut answered = Vec::new();
    for (client, retries) in clients.iter().zip(RETRY_COUNTS) {
        for row in status_rows(status, retries) {
            let url = upstream.url(&row.target());
            let result: Result<ClientResponse, RuntimeError> =
                within_bound(&context, row.send(client, &url)).await;
            answered.push((row, result));
        }
    }
    let free_sends = send_free_verbs(status, &upstream, &context).await;
    let recording = upstream.finish(&context).await;

    let expected: usize = answered
        .iter()
        .map(|(row, _)| row.sends() as usize)
        .sum::<usize>()
        + free_sends;
    assert_eq!(
        recording.requests.len(),
        expected,
        "{context}: the peer read {} requests in all",
        recording.requests.len()
    );
    assert_eq!(
        recording.starts as usize, expected,
        "{context}: the peer saw {} request starts",
        recording.starts
    );
    for (row, result) in answered {
        assert_status_row(row, result, &recording);
    }
    assert_free_verbs_sent_once(status, &recording, &context);
}

/// The retry count of every ambiguous-transport row.
const AMBIGUOUS_RETRIES: u32 = 2;

/// One ambiguous row: the peer reads each request and closes without
/// answering.
///
/// The client cannot tell how far the peer got. An idempotent method is sent
/// again by default, any other method only under `Replayable`, and no method
/// under `Never`. The call ends with the transport failure either way.
async fn assert_ambiguous_row(client: &Client, draft: Draft) {
    let context = format!("ambiguous pre-head close, {draft:?}");
    let expected = sends(draft, true, AMBIGUOUS_RETRIES);
    // Every attempt the count allows reads and closes alike, so a repeat is
    // counted and not parked.
    let script = [Answer::Ambiguous; AMBIGUOUS_RETRIES as usize + 1];
    let mut upstream = ScriptedUpstream::bind(&script).await;

    let result: Result<ClientResponse, RuntimeError> =
        send_draft(client, draft, &upstream.url("/ambiguous-transport")).await;
    let mut read = Vec::new();
    for attempt in 0..expected as usize {
        read.push(upstream.saw(PeerEvent::Started(attempt)).await);
    }
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), &context).await;

    for (attempt, started) in read.into_iter().enumerate() {
        assert_eq!(started, Ok(()), "{context}: attempt {attempt}");
    }
    assert!(
        matches!(result, Err(RuntimeError::Client(ClientError::Transport))),
        "{context}: returned {} instead of the transport failure",
        describe(&Some(result))
    );
    assert_eq!(starts, expected, "{context}: started {starts} requests");
}

/// Every permission and method against the ambiguous peer.
async fn assert_ambiguous_rows() {
    let client = http::client().retries(AMBIGUOUS_RETRIES).backoff(BACKOFF);
    for permission in PERMISSIONS {
        for method in METHODS {
            let draft = Draft::new(method, permission);
            let settled = within_watchdog(assert_ambiguous_row(&client, draft)).await;
            assert!(settled.is_some(), "{draft:?} did not settle");
        }
    }
}

/// The retry count of every row whose outcome no permission repeats.
const FINAL_RETRIES: u32 = 3;

/// The response ceiling for the size-refusal and local-conversion rows.
const FINAL_LIMIT: usize = 8;

/// Admit the truncation fixture's declaration so its incomplete body is read.
const TRUNCATION_LIMIT: usize = 64;

/// A complete answer whose body is twice [`FINAL_LIMIT`].
const OVERSIZED: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n0123456789abcdef";

/// A complete answer no local conversion accepts: a server-fault status and a
/// body that is neither UTF-8 nor JSON.
const UNCONVERTIBLE: &[u8] =
    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\xff\xfe{]";

/// Send one POST under the strongest permission and the row's ceiling, through a
/// client with retries to spare.
///
/// Nothing about the request forbids a repeat, so a single send is the
/// outcome's own doing.
async fn send_replayable_post(url: &str, limit: usize) -> Result<ClientResponse, RuntimeError> {
    http::client()
        .retries(FINAL_RETRIES)
        .backoff(BACKOFF)
        .request("POST", url)?
        .body(payload("POST"))
        .retry_policy(RetryPolicy::Replayable)
        .response_limit(limit)?
        .send()
        .await
}

/// Send one replayable POST at a single scripted final answer, and return the
/// call's result with the request starts the peer saw.
///
/// The fixture is released and joined before the result is returned. A repeat
/// would start a second request, which the peer counts and reports.
async fn final_answer(
    answer: Answer,
    limit: usize,
    reached: PeerEvent,
    context: &str,
) -> (Result<ClientResponse, RuntimeError>, u32) {
    let mut upstream = ScriptedUpstream::bind(&[answer]).await;
    let url = upstream.url("/final-answer");

    let result = within_bound(context, send_replayable_post(&url, limit)).await;
    let started = upstream.saw(PeerEvent::Started(0)).await;
    let answered = upstream.saw(reached).await;
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), context).await;

    assert_eq!(
        started,
        Ok(()),
        "{context}: the peer never read the request"
    );
    assert_eq!(answered, Ok(()), "{context}: the peer never answered");
    (result, starts)
}

/// A final body cut short is the answer: it is never sent for again.
async fn assert_cut_body_is_final() {
    let context = "final body cut short";
    let (result, starts) = final_answer(
        Answer::Truncate,
        TRUNCATION_LIMIT,
        PeerEvent::Truncated(0),
        context,
    )
    .await;

    assert!(
        matches!(result, Err(RuntimeError::Client(ClientError::Transport))),
        "{context}: returned {}",
        describe(&Some(result))
    );
    assert_eq!(starts, 1, "{context}: started {starts} requests");
}

/// A response above its ceiling is the answer: it is never sent for again.
async fn assert_crossed_limit_is_final() {
    let context = "final body above its ceiling";
    let (result, starts) = final_answer(
        Answer::Exact(OVERSIZED),
        FINAL_LIMIT,
        PeerEvent::Answered(0),
        context,
    )
    .await;

    assert!(
        matches!(
            result,
            Err(RuntimeError::Client(ClientError::ResponseTooLarge {
                limit: FINAL_LIMIT
            }))
        ),
        "{context}: returned {}",
        describe(&Some(result))
    );
    assert_eq!(starts, 1, "{context}: started {starts} requests");
}

/// Status, text, and JSON conversion are local: each refuses the collected
/// answer, and none sends anything.
///
/// The fixture is finished before the conversions run, so its count is exact
/// for the send. The conversions then run with no peer left to reach.
async fn assert_local_conversion_sends_nothing() {
    let context = "local conversion of a collected answer";
    let (result, starts) = final_answer(
        Answer::Exact(UNCONVERTIBLE),
        FINAL_LIMIT,
        PeerEvent::Answered(0),
        context,
    )
    .await;
    let response: ClientResponse = answered(context, result);

    assert_eq!(response.status(), 500, "{context}: collected status");
    assert_eq!(starts, 1, "{context}: started {starts} requests");
    assert!(
        matches!(
            response.text(),
            Err(RuntimeError::Client(ClientError::Decode(_)))
        ),
        "{context}: text conversion accepted bytes that are not UTF-8"
    );
    assert!(
        matches!(
            response.json::<serde_json::Value>(),
            Err(RuntimeError::Client(ClientError::Decode(_)))
        ),
        "{context}: JSON conversion accepted bytes that are not JSON"
    );
    assert!(
        matches!(
            response.error_for_status(),
            Err(RuntimeError::Client(ClientError::Status(500)))
        ),
        "{context}: status conversion accepted a 500"
    );
}

/// The client count the sibling rows share.
const SIBLING_RETRIES: u32 = 2;

/// Send one GET with its own retry count, or with the client's.
async fn get_with_count(
    client: &Client,
    url: &str,
    request_count: Option<u32>,
) -> Result<ClientResponse, RuntimeError> {
    let draft = Draft {
        retries: request_count,
        ..Draft::new("GET", Permission::Defaulted)
    };
    send_draft(client, draft, url).await
}

/// Concurrent requests through one client keep their own permission and count.
///
/// Every request gets the same transient status. The replayable POST spends
/// the client's count, and its siblings in flight beside it do not: the POST
/// that set nothing, the GET that set `Never`, and the GET that set its own
/// count to zero. The client is then used again, and its own default and count
/// are unchanged.
async fn assert_sibling_permissions_are_isolated() {
    let context = "concurrent sibling permissions";
    let upstream = RecordingUpstream::bind(status_reply(503)).await;
    let client = http::client().retries(SIBLING_RETRIES).backoff(BACKOFF);
    let replayable_url = upstream.url("/sibling/replayable");
    let defaulted_url = upstream.url("/sibling/defaulted");
    let never_url = upstream.url("/sibling/never");
    let zero_url = upstream.url("/sibling/zero");
    let later_post_url = upstream.url("/sibling/later-post");
    let later_get_url = upstream.url("/sibling/later-get");

    let siblings = within_bound(context, async {
        tokio::join!(
            send_draft(
                &client,
                Draft::new("POST", Permission::Replayable),
                &replayable_url
            ),
            send_draft(
                &client,
                Draft::new("POST", Permission::Defaulted),
                &defaulted_url
            ),
            send_draft(&client, Draft::new("GET", Permission::Never), &never_url),
            get_with_count(&client, &zero_url, Some(0)),
        )
    })
    .await;
    let later_post = within_bound(context, client.post(&later_post_url, "post")).await;
    let later_get = within_bound(context, client.get(&later_get_url)).await;
    let recording = upstream.finish(context).await;

    let (replayable, defaulted, never, zero) = siblings;
    for (label, result) in [
        ("replayable", replayable),
        ("defaulted", defaulted),
        ("never", never),
        ("zero", zero),
        ("later-post", later_post),
        ("later-get", later_get),
    ] {
        let answered: ClientResponse =
            result.unwrap_or_else(|error| panic!("{context}: {label} failed: {error:?}"));
        assert_eq!(answered.status(), 503, "{context}: {label} status");
    }
    for (target, expected) in [
        ("/sibling/replayable", SIBLING_RETRIES as usize + 1),
        ("/sibling/defaulted", 1),
        ("/sibling/never", 1),
        ("/sibling/zero", 1),
        ("/sibling/later-post", 1),
        ("/sibling/later-get", SIBLING_RETRIES as usize + 1),
    ] {
        let sent = recording.sent_to(target).len();
        assert_eq!(sent, expected, "{context}: {target} was sent {sent} times");
    }
}

/// A certificate no trust store holds is a terminal TLS failure.
///
/// The request is an idempotent method under the strongest permission with
/// retries to spare, so only the failure's own category stops a second dial.
/// The peer reports every connection it accepts, and one the row did not
/// consume fails the fixture's release.
async fn assert_untrusted_tls_is_not_repeated() {
    let row = "untrusted certificate under a replayable request";
    let mut peer = untrusted_tls_peer().await;
    let url = format!("https://{}/tls-terminal", peer.addr());
    let client = http::client().retries(FINAL_RETRIES).backoff(BACKOFF);

    let result: Result<ClientResponse, RuntimeError> = within_bound(
        row,
        send_draft(&client, Draft::new("GET", Permission::Replayable), &url),
    )
    .await;
    let seen = within_watchdog(peer.recv()).await;
    peer.finish(row, within_watchdog).await;

    assert_eq!(
        seen,
        Some(Some(TlsPeerEvent::HandshakeRefused)),
        "{row}: the client must refuse the one handshake it began"
    );
    assert!(
        matches!(result, Err(RuntimeError::Client(ClientError::Tls))),
        "{row}: returned {} instead of the TLS failure",
        describe(&Some(result))
    );
}

/// Every row that runs on real time.
async fn live_rows() {
    let clients = RETRY_COUNTS.map(|retries| http::client().retries(retries).backoff(BACKOFF));
    for status in TRANSIENT_STATUSES.into_iter().chain(SETTLED_STATUSES) {
        assert_status_rows(status, &clients).await;
    }
    assert_ambiguous_rows().await;
    assert_cut_body_is_final().await;
    assert_crossed_limit_is_final().await;
    assert_local_conversion_sends_nothing().await;
    assert_sibling_permissions_are_isolated().await;
    assert_untrusted_tls_is_not_repeated().await;
}

/// The bound each timeout row places on one attempt.
const ATTEMPT: Duration = Duration::from_millis(50);

/// The base backoff of every timeout row.
const TIMEOUT_BACKOFF: Duration = Duration::from_millis(10);

/// The timer that ends each attempt of one timeout row.
#[derive(Clone, Copy, Debug)]
enum AttemptTimer {
    /// The attempt timer Camber owns.
    Attempt,
    /// The quiet-interval timer the transport owns.
    Transport,
}

/// Every timer that can end an attempt before its response head.
const ATTEMPT_TIMERS: [AttemptTimer; 2] = [AttemptTimer::Attempt, AttemptTimer::Transport];

impl AttemptTimer {
    /// A single-retry client whose only bound short enough to matter is this
    /// timer's.
    fn client(self) -> Client {
        let client = http::client().retries(1).backoff(TIMEOUT_BACKOFF);
        match self {
            Self::Attempt => client.request_timeout(ATTEMPT),
            Self::Transport => client.response_idle_timeout(ATTEMPT),
        }
    }

    /// The timeout a call ended by this timer returns.
    fn timeout(self) -> ClientTimeout {
        match self {
            Self::Attempt => ClientTimeout::Attempt,
            Self::Transport => ClientTimeout::Transport,
        }
    }

    /// The step from a parked attempt to this timer's expiry.
    ///
    /// The transport arms a relative sleep, and Tokio rounds that up to the
    /// next millisecond. The timer Camber owns is stepped onto exactly.
    fn step(self) -> Duration {
        match self {
            Self::Attempt => ATTEMPT,
            Self::Transport => ATTEMPT + TIMER_ROUNDING,
        }
    }

    /// Whether `result` is this timer's own timeout.
    fn ended(self, result: &Option<Result<ClientResponse, RuntimeError>>) -> bool {
        matches!(
            result,
            Some(Err(RuntimeError::Client(ClientError::Timeout(observed))))
                if *observed == self.timeout()
        )
    }
}

/// Step the clock over the delay and onto the second attempt's own bound.
///
/// The delay is stepped over only after the peer has seen the first attempt's
/// transport closed. The second attempt is stepped onto its bound only after
/// the peer has parked it.
async fn cross_second_attempt(upstream: &mut ScriptedUpstream, timer: AttemptTimer, context: &str) {
    tokio::time::advance(2 * TIMEOUT_BACKOFF).await;
    upstream
        .expect_each(&[PeerEvent::Started(1), PeerEvent::Stalled(1)], context)
        .await;
    tokio::time::advance(timer.step()).await;
}

/// Step the clock past every delay and bound a single-retry call could hold.
///
/// A call that kept a retry scheduled after its answer would send here. The
/// peer counts that start and holds its report, so the row fails at cleanup.
async fn step_past_a_scheduled_retry() {
    tokio::time::advance(2 * (ATTEMPT + TIMER_ROUNDING + 2 * TIMEOUT_BACKOFF)).await;
    settle_wakeups().await;
}

/// One timeout row: every head is held past the bound `timer` places on the
/// attempt.
///
/// The timer says how long the attempt took and nothing about how far it got,
/// whichever owner armed it. A permitted request is sent once more and ends on
/// the second attempt's timer. Any other request ends on the first, and
/// nothing is sent after it.
async fn assert_attempt_timeout_row(timer: AttemptTimer, draft: Draft) {
    let context = format!("{timer:?} timeout, {draft:?}");
    let replayed = draft.permission.permits(draft.method);
    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallHead, Answer::StallHead]).await;
    let call = spawn_draft(timer.client(), draft, upstream.url("/slow"));

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], &context)
        .await;
    tokio::time::advance(timer.step()).await;
    upstream.expect(PeerEvent::Released(0), &context).await;
    if replayed {
        cross_second_attempt(&mut upstream, timer, &context).await;
    }
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = match replayed {
        true => upstream.saw(PeerEvent::Released(1)).await,
        false => Ok(()),
    };
    step_past_a_scheduled_retry().await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        timer.ended(&result),
        "{context}: expected {:?}, got {}",
        timer.timeout(),
        describe(&result)
    );
    assert_released(released, &context);
    assert_eq!(
        starts,
        sends(draft, true, 1),
        "{context}: the call started {starts} requests"
    );
}

/// Every row that holds the clock still.
async fn timer_rows() {
    for timer in ATTEMPT_TIMERS {
        timer_row_set(timer).await;
    }
}

/// One timer's rows: every permission with every method.
async fn timer_row_set(timer: AttemptTimer) {
    for permission in PERMISSIONS {
        for method in METHODS {
            assert_attempt_timeout_row(timer, Draft::new(method, permission)).await;
        }
    }
}

/// Outbound 5.T1: a send after the first needs a configured count, a transient
/// outcome before the response head, and this request's own permission.
///
/// The real-time rows and the held-clock rows need different runtimes, so the
/// test owns both and runs them in turn.
#[test]
fn request_local_replay_controls_every_additional_send() {
    let live = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    live.block_on(live_rows());
    drop(live);

    let held = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    held.block_on(timer_rows());
}

const GENERATED_RETRY_CASES: u64 = 24;
const RETRY_COUNT_BOUND: NonZeroUsize = NonZeroUsize::new(5).unwrap();

/// The generated categories a run must reach, or the family proves only one
/// owner of the count.
const COUNT_CATEGORIES: [&str; 2] = ["client-count", "request-count"];

/// A request's own count replaces its client's, and each count is exactly the
/// sends after the first.
#[tokio::test(flavor = "multi_thread")]
async fn generated_retry_attempt_arithmetic_is_exact() {
    let calls = Arc::new(AtomicU32::new(0));
    let handler_calls = Arc::clone(&calls);
    let mut router = Router::new();
    router.get("/generated-retry", move |_req: &Request| {
        handler_calls.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let url = format!("http://{}/generated-retry", server.local_addr());
    let generator = crate::deterministic::DeterministicGenerator::stable();
    let mut reached = BTreeSet::new();

    for index in 0..GENERATED_RETRY_CASES {
        let mut case = generator.case(index);
        let client_count = case.bounded(RETRY_COUNT_BOUND) as u32;
        let request_count = case
            .boolean()
            .then(|| case.bounded(RETRY_COUNT_BOUND) as u32);
        let (effective, category) = match request_count {
            Some(count) => (count, "request-count"),
            None => (client_count, "client-count"),
        };
        let client = http::client().retries(client_count).backoff(BACKOFF);
        calls.store(0, Ordering::Relaxed);

        let response: ClientResponse = get_with_count(&client, &url, request_count).await.unwrap();

        let context = format!("{case}: client={client_count} request={request_count:?}");
        assert_eq!(response.status(), 503, "{context}");
        assert_eq!(calls.load(Ordering::Relaxed), effective + 1, "{context}");
        reached.insert(category);
    }
    generator.assert_reached(GENERATED_RETRY_CASES, COUNT_CATEGORIES, &reached);

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

/// The base backoff of the disposal rows.
const DISPOSAL_BACKOFF: Duration = Duration::from_millis(500);

/// One disposal row: the transient answer's connection closes while the clock
/// is held still, so the release cannot have waited for the delay to end.
///
/// The delay is stepped over only after the peer has seen that release.
async fn assert_released_before_backoff(draft: Draft) {
    let context = format!("transient release before backoff, {draft:?}");
    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::Transient, Answer::Complete]).await;
    let client = http::client().retries(1).backoff(DISPOSAL_BACKOFF);
    let call = spawn_draft(client, draft, upstream.url("/retry-release"));

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Disposed(0)], &context)
        .await;
    tokio::time::advance(2 * DISPOSAL_BACKOFF).await;
    upstream.finish_answered(1, call, driver, &context).await;
}

/// A transient answer is released before the delay, by default for an
/// idempotent method and under `Replayable` for a method with a body.
#[tokio::test(start_paused = true)]
async fn transient_response_is_released_before_retry_backoff() {
    for draft in [
        Draft::new("GET", Permission::Defaulted),
        Draft::new("PUT", Permission::Idempotent),
        Draft::new("POST", Permission::Replayable),
    ] {
        assert_released_before_backoff(draft).await;
    }
}
