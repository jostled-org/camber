//! Outbound 6.T1 and 6.T2: a redirect is followed only to the origin the call
//! began at, and every send it adds is one the request's own policies allow.
//!
//! The authority rows read two recording peers on two loopback ports. The
//! first scripts each redirect by target. The second is the place no call may
//! reach. The method rows read one recording peer, so each row states the
//! exact method, body, and headers of every send. The held rows read a
//! scripted peer that never sends a redirect's declared body. No row reads
//! elapsed time as evidence.

use crate::bounded_client::within_bound;
use crate::error_chain::assert_safe;
use crate::retry_policy::{Draft, METHODS, PERMISSIONS, Permission, describe_draft};
use crate::retry_upstream::{
    Answer, PROBE_TARGET, PeerEvent, Recorded, Recording, RecordingUpstream, RunnableDriver,
    ScriptedUpstream, answer, answered, assert_released, describe, describe_result, payload, probe,
    settle_wakeups, settled, spawn_get,
};

use camber::RuntimeError;
use camber::http;
use camber::http::Client;
use camber::http::ClientError;
use camber::http::ClientRedirect;
use camber::http::ClientRequest;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::RedirectPolicy;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// The credential every row sends, marked sensitive.
const TOKEN: &str = "Bearer credential-sentinel-9b2e";

/// The application header every row sends.
const TENANT: &str = "tenant-sentinel-41aa";

/// The text no error rendering may repeat: the credential, the tenant, and the
/// sentinels the refused `Location` values of [`ROUTES`] carry.
const SECRETS: [&str; 4] = [
    "credential-sentinel-9b2e",
    TENANT,
    "location-sentinel-5f1c",
    "userinfo-sentinel-c07d",
];

/// The body a redirect answer carries.
const REDIRECT_BODY: &[u8] = b"redirect";

/// The body the landing answer carries. It is not UTF-8, so a returned copy
/// proves the bytes and not a text reading of them.
const LANDED_BODY: &[u8] = b"landed\xff\x00bytes";

/// A response ceiling below [`REDIRECT_BODY`].
const NARROW_LIMIT: usize = 4;

/// The base backoff of the rows that run on real time.
const BACKOFF: Duration = Duration::from_millis(1);

/// The policy that follows at most `hops` same-origin redirects.
fn same_origin(hops: usize) -> RedirectPolicy {
    RedirectPolicy::SameOrigin {
        max_hops: NonZeroUsize::new(hops).unwrap(),
    }
}

/// One scripted answer of the first origin.
#[derive(Clone, Copy)]
struct Route {
    target: &'static str,
    status_line: &'static str,
    /// One `Location` header per entry. `{authority}` is the host and port the
    /// request named, `{port}` is that port, and `{second_port}` is the port
    /// of the second origin.
    locations: &'static [&'static str],
    body: &'static [u8],
}

impl Route {
    /// A redirect answer with one `Location` per entry of `locations`.
    const fn redirect(
        target: &'static str,
        status_line: &'static str,
        locations: &'static [&'static str],
    ) -> Self {
        Self {
            target,
            status_line,
            locations,
            body: REDIRECT_BODY,
        }
    }

    /// A 302 answer with one `Location`.
    const fn found(target: &'static str, location: &'static [&'static str]) -> Self {
        Self::redirect(target, "302 Found", location)
    }

    /// The wire image of this answer for a request that named `authority`.
    fn reply(&self, authority: &str, second_port: u16) -> Vec<u8> {
        let port = authority.rsplit(':').next().unwrap_or_default();
        let locations: Vec<String> = self
            .locations
            .iter()
            .map(|location| {
                location
                    .replace("{authority}", authority)
                    .replace("{second_port}", &second_port.to_string())
                    .replace("{port}", port)
            })
            .collect();
        let headers: Vec<(&str, &str)> = locations
            .iter()
            .map(|location| ("Location", location.as_str()))
            .collect();
        answer(self.status_line, &headers, self.body)
    }
}

/// Every redirect the first origin scripts. A target under `/landed/` that is
/// not listed here is the landing answer.
const ROUTES: &[Route] = &[
    Route::found("/none/default", &["/landed/none"]),
    Route::found("/none/request", &["/landed/none"]),
    Route::found("/none/client", &["/landed/none"]),
    Route::found("/none/request-over-client", &["/landed/none"]),
    Route::found("/none/limited", &["/landed/none"]),
    Route::found("/relative/path", &["/landed/path?q=1#fragment-sentinel"]),
    Route::redirect(
        "/relative/dir/start",
        "307 Temporary Redirect",
        &["next#fragment-sentinel"],
    ),
    Route::found("/relative/dir/next", &["../../landed/dir"]),
    Route::redirect(
        "/absolute/start",
        "301 Moved Permanently",
        &["http://{authority}/landed/absolute"],
    ),
    Route::redirect(
        "/scheme-relative/start",
        "308 Permanent Redirect",
        &["//{authority}/landed/scheme-relative"],
    ),
    Route::redirect("/see-other/start", "303 See Other", &["/landed/see-other"]),
    Route::found("/chain/exact/2", &["/chain/exact/1"]),
    Route::found("/chain/exact/1", &["/landed/chain-exact"]),
    Route::found("/chain/over/3", &["/chain/over/2"]),
    Route::found("/chain/over/2", &["/chain/over/1"]),
    Route::found("/chain/over/1", &["/landed/chain-over"]),
    Route::found("/loop", &["/loop"]),
    Route::found("/missing", &[]),
    Route::found("/missing/limited", &[]),
    Route::redirect(
        "/unfollowed/300",
        "300 Multiple Choices",
        &["/landed/unfollowed"],
    ),
    // A 304 has no body on the wire, whatever its head declares.
    Route {
        target: "/unfollowed/304",
        status_line: "304 Not Modified",
        locations: &["/landed/unfollowed"],
        body: b"",
    },
    Route::redirect("/unfollowed/305", "305 Use Proxy", &["/landed/unfollowed"]),
    Route::redirect("/unfollowed/306", "306 Unused", &["/landed/unfollowed"]),
    Route::found(
        "/refused/repeated",
        &[
            "/landed/repeated/location-sentinel-5f1c",
            "/landed/repeated/second",
        ],
    ),
    Route::found("/refused/bracket", &["http://[::1/location-sentinel-5f1c"]),
    Route::found(
        "/refused/space",
        &["http://bad host/location-sentinel-5f1c"],
    ),
    Route::found(
        "/refused/userinfo",
        &["http://user:userinfo-sentinel-c07d@{authority}/landed/userinfo"],
    ),
    Route::found(
        "/refused/ftp",
        &["ftp://{authority}/location-sentinel-5f1c"],
    ),
    Route::found(
        "/refused/mailto",
        &["mailto:location-sentinel-5f1c@example.invalid"],
    ),
    Route::found("/cross/port", &["http://127.0.0.1:{second_port}/elsewhere"]),
    Route::found("/cross/host", &["http://localhost:{port}/elsewhere"]),
    Route::found(
        "/cross/host-and-port",
        &["http://localhost:{second_port}/elsewhere"],
    ),
    Route::found("/cross/scheme", &["https://{authority}/elsewhere"]),
    Route::found("/cross/scheme-relative", &["//localhost:{port}/elsewhere"]),
    Route::found("/cross/second-hop", &["/cross/second-hop/next"]),
    Route::found(
        "/cross/second-hop/next",
        &["http://127.0.0.1:{second_port}/elsewhere"],
    ),
    Route::found(
        "/sibling/following/none",
        &["/landed/sibling/following/none"],
    ),
    Route::found(
        "/sibling/following/defaulted",
        &["/landed/sibling/following/defaulted"],
    ),
    Route::found(
        "/sibling/following/later",
        &["/landed/sibling/following/later"],
    ),
    Route::found(
        "/sibling/returning/same-origin",
        &["/landed/sibling/returning/same-origin"],
    ),
    Route::found(
        "/sibling/returning/defaulted",
        &["/landed/sibling/returning/defaulted"],
    ),
    Route::found(
        "/sibling/returning/later",
        &["/landed/sibling/returning/later"],
    ),
];

/// The first origin's answer to one request.
///
/// A target that is neither scripted nor a landing gets a status no row
/// expects, so a send the rows did not predict fails the row that made it.
fn first_origin_reply(request: &Recorded, second_port: u16) -> Vec<u8> {
    let hosts = request.header_values("host");
    let authority = hosts.first().copied().unwrap_or_default();
    let route = ROUTES.iter().find(|route| route.target == request.target());
    match route {
        Some(route) => route.reply(authority, second_port),
        None if request.target().starts_with("/landed/") => {
            answer("200 OK", &[("X-Landed", "yes")], LANDED_BODY)
        }
        None => answer("599 Unscripted", &[], b""),
    }
}

/// Where a row writes its redirect policy.
#[derive(Clone, Copy, Debug)]
enum Placement {
    /// Nowhere, so the documented default applies.
    Defaulted,
    /// `None` on the request.
    RequestNone,
    /// `None` on the client.
    ClientNone,
    /// `None` on the request, over a client that follows.
    RequestNoneOverClient,
    /// This many same-origin hops on the request.
    Request(usize),
    /// This many same-origin hops on the client.
    Client(usize),
}

impl Placement {
    /// The client this row sends through.
    fn client(self) -> Client {
        match self {
            Self::Defaulted | Self::RequestNone | Self::Request(_) => http::client(),
            Self::ClientNone => http::client().redirect_policy(RedirectPolicy::None),
            Self::RequestNoneOverClient => http::client().redirect_policy(same_origin(4)),
            Self::Client(hops) => http::client().redirect_policy(same_origin(hops)),
        }
    }

    /// The request with this row's own policy written on it, if it has one.
    fn describe(self, request: ClientRequest<'_>) -> ClientRequest<'_> {
        match self {
            Self::Defaulted | Self::ClientNone | Self::Client(_) => request,
            Self::RequestNone | Self::RequestNoneOverClient => {
                request.redirect_policy(RedirectPolicy::None)
            }
            Self::Request(hops) => request.redirect_policy(same_origin(hops)),
        }
    }
}

/// What one authority row's call returns.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// The scripted answer itself, redirect or not.
    Returned {
        status: u16,
        location: Option<&'static str>,
        body: &'static [u8],
    },
    /// The landing answer, reached through every scripted hop.
    Landed,
    /// The row's own response ceiling, crossed by the returned answer.
    TooLarge,
    /// A redirect refusal of one of these kinds.
    Refused(&'static [ClientRedirect]),
}

/// The refusal of a `Location` that is malformed, repeated, or at the first
/// origin with a user name.
const INVALID: &[ClientRedirect] = &[ClientRedirect::InvalidLocation];

/// The refusal of a `Location` that names another origin. A scheme that is
/// not HTTP is another origin.
const CHANGED: &[ClientRedirect] = &[ClientRedirect::OriginChanged];

/// The refusal of a redirect past the allowance.
const SPENT: &[ClientRedirect] = &[ClientRedirect::HopLimit];

/// One authority row: a GET to the first target of `sent`.
#[derive(Clone, Copy, Debug)]
struct Row {
    label: &'static str,
    placement: Placement,
    /// The response ceiling written on the request, or `None` to keep the
    /// client's.
    limit: Option<usize>,
    expect: Expect,
    /// Every target the first origin reads for this row, in order.
    sent: &'static [&'static str],
}

impl Row {
    /// A row whose redirect is the answer, under `placement`.
    const fn returned(
        label: &'static str,
        placement: Placement,
        sent: &'static [&'static str],
    ) -> Self {
        Self {
            label,
            placement,
            limit: None,
            expect: Expect::Returned {
                status: 302,
                location: Some("/landed/none"),
                body: REDIRECT_BODY,
            },
            sent,
        }
    }

    /// A row that follows every hop of `sent` to the landing answer.
    const fn followed(label: &'static str, hops: usize, sent: &'static [&'static str]) -> Self {
        Self {
            label,
            placement: Placement::Request(hops),
            limit: None,
            expect: Expect::Landed,
            sent,
        }
    }

    /// A row that is refused after reading `sent`.
    const fn refused(
        label: &'static str,
        kinds: &'static [ClientRedirect],
        sent: &'static [&'static str],
    ) -> Self {
        Self {
            expect: Expect::Refused(kinds),
            ..Self::followed(label, 4, sent)
        }
    }

    /// A row whose answer is not a followed redirect, under a policy that
    /// follows.
    const fn ordinary(
        label: &'static str,
        status: u16,
        location: Option<&'static str>,
        body: &'static [u8],
        sent: &'static [&'static str],
    ) -> Self {
        Self {
            expect: Expect::Returned {
                status,
                location,
                body,
            },
            ..Self::followed(label, 4, sent)
        }
    }
}

/// Every authority row, in the order it is sent.
const ROWS: &[Row] = &[
    Row::returned("default policy", Placement::Defaulted, &["/none/default"]),
    Row::returned(
        "None on the request",
        Placement::RequestNone,
        &["/none/request"],
    ),
    Row::returned(
        "None on the client",
        Placement::ClientNone,
        &["/none/client"],
    ),
    Row::returned(
        "None on the request over a following client",
        Placement::RequestNoneOverClient,
        &["/none/request-over-client"],
    ),
    Row {
        label: "returned redirect above its ceiling",
        placement: Placement::Defaulted,
        limit: Some(NARROW_LIMIT),
        expect: Expect::TooLarge,
        sent: &["/none/limited"],
    },
    Row::followed(
        "absolute-path Location with a fragment",
        4,
        &["/relative/path", "/landed/path?q=1"],
    ),
    Row::followed(
        "Location relative to the current hop",
        4,
        &["/relative/dir/start", "/relative/dir/next", "/landed/dir"],
    ),
    Row {
        placement: Placement::Client(4),
        ..Row::followed(
            "absolute same-origin Location under the client's policy",
            4,
            &["/absolute/start", "/landed/absolute"],
        )
    },
    Row::followed(
        "scheme-relative same-origin Location",
        4,
        &["/scheme-relative/start", "/landed/scheme-relative"],
    ),
    Row::followed(
        "303 for a GET",
        4,
        &["/see-other/start", "/landed/see-other"],
    ),
    Row::followed(
        "as many redirects as the allowance",
        2,
        &["/chain/exact/2", "/chain/exact/1", "/landed/chain-exact"],
    ),
    Row {
        placement: Placement::Request(2),
        ..Row::refused(
            "one redirect more than the allowance",
            SPENT,
            &["/chain/over/3", "/chain/over/2", "/chain/over/1"],
        )
    },
    Row {
        placement: Placement::Client(1),
        ..Row::refused("redirect loop", SPENT, &["/loop", "/loop"])
    },
    Row::ordinary("no Location", 302, None, REDIRECT_BODY, &["/missing"]),
    Row {
        limit: Some(NARROW_LIMIT),
        expect: Expect::TooLarge,
        ..Row::followed("no Location above its ceiling", 4, &["/missing/limited"])
    },
    Row::ordinary(
        "300 with a Location",
        300,
        Some("/landed/unfollowed"),
        REDIRECT_BODY,
        &["/unfollowed/300"],
    ),
    Row::ordinary(
        "304 with a Location",
        304,
        Some("/landed/unfollowed"),
        b"",
        &["/unfollowed/304"],
    ),
    Row::ordinary(
        "305 with a Location",
        305,
        Some("/landed/unfollowed"),
        REDIRECT_BODY,
        &["/unfollowed/305"],
    ),
    Row::ordinary(
        "306 with a Location",
        306,
        Some("/landed/unfollowed"),
        REDIRECT_BODY,
        &["/unfollowed/306"],
    ),
    Row::refused("repeated Location", INVALID, &["/refused/repeated"]),
    Row::refused("unclosed IPv6 host", INVALID, &["/refused/bracket"]),
    Row::refused("space in the host", INVALID, &["/refused/space"]),
    Row::refused("userinfo", INVALID, &["/refused/userinfo"]),
    Row::refused("FTP scheme", CHANGED, &["/refused/ftp"]),
    Row::refused("mailto scheme", CHANGED, &["/refused/mailto"]),
    Row::refused("another port", CHANGED, &["/cross/port"]),
    Row::refused("another host", CHANGED, &["/cross/host"]),
    Row::refused("another host and port", CHANGED, &["/cross/host-and-port"]),
    Row::refused("another scheme", CHANGED, &["/cross/scheme"]),
    Row::refused(
        "scheme-relative Location to another host",
        CHANGED,
        &["/cross/scheme-relative"],
    ),
    Row::refused(
        "another port after an authorized hop",
        CHANGED,
        &["/cross/second-hop", "/cross/second-hop/next"],
    ),
];

/// Send one authority row's GET at the first origin.
async fn send_row(row: &Row, first: &RecordingUpstream) -> Result<ClientResponse, RuntimeError> {
    let client: Client = row.placement.client();
    let url = first.url(row.sent[0]);
    let request: ClientRequest<'_> = client
        .request("GET", &url)?
        .sensitive_header("authorization", TOKEN)?
        .header("x-tenant", TENANT)?;
    let request = row.placement.describe(request);
    let request = match row.limit {
        Some(limit) => request.response_limit(limit)?,
        None => request,
    };
    request.send().await
}

/// Require a refusal of one of `kinds` that repeats no secret.
fn assert_refused(
    row: &Row,
    kinds: &[ClientRedirect],
    result: Result<ClientResponse, RuntimeError>,
) {
    let label = row.label;
    match result {
        Err(RuntimeError::Client(ClientError::Redirect(kind))) if kinds.contains(&kind) => {
            let error = RuntimeError::Client(ClientError::Redirect(kind));
            assert_safe(&error, &SECRETS, label);
        }
        other => panic!(
            "{label}: returned {} instead of a refusal in {kinds:?}",
            describe_result(&other)
        ),
    }
}

/// Require the answer one authority row expects.
fn assert_row(row: &Row, result: Result<ClientResponse, RuntimeError>) {
    let label = row.label;
    let (status, location, body) = match row.expect {
        Expect::Refused(kinds) => return assert_refused(row, kinds, result),
        Expect::TooLarge => {
            assert!(
                matches!(
                    result,
                    Err(RuntimeError::Client(ClientError::ResponseTooLarge { limit }))
                        if Some(limit) == row.limit
                ),
                "{label}: returned {} instead of the crossed ceiling",
                describe_result(&result)
            );
            return;
        }
        Expect::Returned {
            status,
            location,
            body,
        } => (status, location, body),
        Expect::Landed => (200, None, LANDED_BODY),
    };
    let response: ClientResponse = answered(label, result);
    assert_eq!(response.status(), status, "{label}: status");
    assert_eq!(
        response.header("location"),
        location.map(str::as_bytes),
        "{label}: Location"
    );
    assert_eq!(response.body_bytes(), body, "{label}: body");
}

/// Require the first origin to have read exactly the sends the rows predict,
/// each with the credential and the application header, and nothing else.
fn assert_first_origin(recording: &Recording) {
    let read: Vec<&str> = recording.requests.iter().map(Recorded::target).collect();
    let expected: Vec<&str> = ROWS.iter().flat_map(|row| row.sent).copied().collect();
    assert_eq!(
        read, expected,
        "the first origin read other sends than the rows allow"
    );
    for request in &recording.requests {
        let target = request.target();
        assert_eq!(request.method(), "GET", "{target}: method");
        assert_eq!(
            *request.header_values("authorization"),
            [TOKEN],
            "{target}: the credential must reach every authorized hop once"
        );
        assert_eq!(
            *request.header_values("x-tenant"),
            [TENANT],
            "{target}: the application header must reach every authorized hop once"
        );
        assert!(
            request.header_values("referer").is_empty(),
            "{target}: the client added a Referer"
        );
    }
}

/// Require the second origin to have accepted and read only the row's probe.
fn assert_second_origin_untouched(recording: &Recording) {
    let read: Vec<&str> = recording.requests.iter().map(Recorded::target).collect();
    assert_eq!(
        recording.connections, 1,
        "the second origin accepted {} connections, and only the probe may dial it",
        recording.connections
    );
    assert_eq!(
        recording.starts, 1,
        "the second origin saw {} request starts, and only the probe may send",
        recording.starts
    );
    assert_eq!(
        read,
        [PROBE_TARGET],
        "the second origin read a request no policy authorized"
    );
}

/// One request of a sibling row.
#[derive(Clone, Debug)]
struct Sibling {
    /// The target the request names, under `/sibling/`.
    target: &'static str,
    /// The policy written on the request, or `None` to keep the client's.
    policy: Option<RedirectPolicy>,
    /// Whether the policy in effect for this request follows its redirect.
    lands: bool,
}

impl Sibling {
    /// The target the redirect this request gets names.
    fn landing(&self) -> String {
        format!("/landed{}", self.target)
    }

    /// Send this request's GET through the shared `client`.
    async fn send(
        &self,
        client: &Client,
        first: &RecordingUpstream,
    ) -> Result<ClientResponse, RuntimeError> {
        let request: ClientRequest<'_> = client.request("GET", &first.url(self.target))?;
        let request = match &self.policy {
            Some(policy) => request.redirect_policy(policy.clone()),
            None => request,
        };
        request.send().await
    }

    /// Require this request's own answer and its own sends at the peer.
    fn assert(
        &self,
        context: &str,
        result: Result<ClientResponse, RuntimeError>,
        recording: &Recording,
    ) {
        let target = self.target;
        let landing = self.landing();
        let response: ClientResponse =
            result.unwrap_or_else(|error| panic!("{context}: {target} failed: {error:?}"));
        let (status, location, body, landed) = match self.lands {
            true => (200, None, LANDED_BODY, 1),
            false => (302, Some(landing.as_bytes()), REDIRECT_BODY, 0),
        };
        assert_eq!(response.status(), status, "{context}: {target} status");
        assert_eq!(
            response.header("location"),
            location,
            "{context}: {target} Location"
        );
        assert_eq!(response.body_bytes(), body, "{context}: {target} body");
        assert_eq!(
            recording.sent_to(target).len(),
            1,
            "{context}: {target} was not sent exactly once"
        );
        assert_eq!(
            recording.sent_to(&landing).len(),
            landed,
            "{context}: {target} must reach its landing {landed} times"
        );
    }
}

/// Send two concurrent requests and one later request through one `client`,
/// and require each to have run under its own policy.
///
/// `first` writes its own policy and `second` keeps the client's. `later` is
/// a convenience GET, sent after both have settled, so it reads what the
/// client holds once a request has overridden it.
async fn assert_siblings(context: &str, client: Client, siblings: [Sibling; 3]) {
    let first_origin =
        RecordingUpstream::bind_responder(|request| first_origin_reply(request, 0)).await;
    let [first, second, later] = &siblings;

    let (first_result, second_result) = within_bound(context, async {
        tokio::join!(
            first.send(&client, &first_origin),
            second.send(&client, &first_origin),
        )
    })
    .await;
    let later_result: Result<ClientResponse, RuntimeError> =
        within_bound(context, client.get(&first_origin.url(later.target))).await;
    let recording = first_origin.finish(context).await;

    let sends = siblings.iter().filter(|sibling| sibling.lands).count() + siblings.len();
    assert_eq!(
        recording.requests.len(),
        sends,
        "{context}: the peer read {:?}",
        recording.requests
    );
    first.assert(context, first_result, &recording);
    second.assert(context, second_result, &recording);
    later.assert(context, later_result, &recording);
}

/// A request's redirect policy is its own, in both directions.
///
/// Under a client that follows, the request that wrote `None` gets its
/// redirect back while the sibling in flight beside it lands. Under a client
/// that keeps the default, the request that wrote `SameOrigin` lands while its
/// sibling gets the redirect back. The client is then used again, and its own
/// policy is unchanged.
async fn assert_sibling_policies_are_isolated() {
    assert_siblings(
        "None on a request beside a following sibling",
        http::client().redirect_policy(same_origin(4)),
        [
            Sibling {
                target: "/sibling/following/none",
                policy: Some(RedirectPolicy::None),
                lands: false,
            },
            Sibling {
                target: "/sibling/following/defaulted",
                policy: None,
                lands: true,
            },
            Sibling {
                target: "/sibling/following/later",
                policy: None,
                lands: true,
            },
        ],
    )
    .await;
    assert_siblings(
        "SameOrigin on a request beside a defaulted sibling",
        http::client(),
        [
            Sibling {
                target: "/sibling/returning/same-origin",
                policy: Some(same_origin(4)),
                lands: true,
            },
            Sibling {
                target: "/sibling/returning/defaulted",
                policy: None,
                lands: false,
            },
            Sibling {
                target: "/sibling/returning/later",
                policy: None,
                lands: false,
            },
        ],
    )
    .await;
}

/// Outbound 6.T1: under `None` a redirect is the answer. Under `SameOrigin` a
/// call follows a redirect only to its first origin and only within its
/// allowance, and refuses every other destination before it dials. A policy
/// written on one request changes no sibling through the same client.
///
/// The second origin is inspected only after every call has settled. The
/// row's own probe is then the one connection it accepted.
#[tokio::test]
async fn redirect_policy_rejects_unauthorized_second_destinations() {
    let context = "redirect authority";
    let second = RecordingUpstream::bind(answer("200 OK", &[], b"")).await;
    let second_addr = second.addr();
    let first = RecordingUpstream::bind_responder(move |request| {
        first_origin_reply(request, second_addr.port())
    })
    .await;
    let mut results = Vec::new();

    for row in ROWS {
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(row.label, send_row(row, &first)).await;
        results.push(result);
    }
    let probed = within_bound(context, probe(second_addr)).await;
    let first = first.finish(context).await;
    let second = second.finish(context).await;

    assert_eq!(probed, Ok(()), "{context}: the probe failed");
    for (row, result) in ROWS.iter().zip(results) {
        assert_row(row, result);
    }
    assert_first_origin(&first);
    assert_second_origin_untouched(&second);
    assert_sibling_policies_are_isolated().await;
}

/// The content type every method row declares for a body it sends.
const CONTENT_TYPE: &str = "text/plain";

/// The redirect statuses a `SameOrigin` policy follows.
const FOLLOWED_STATUSES: [u16; 5] = [301, 302, 303, 307, 308];

/// The retry count of the method rows' client. Every landing answers 200, so
/// a second send of a first request is a refusal or a redirect sent again.
const TABLE_RETRIES: u32 = 2;

/// The redirect allowance of every method row.
const TABLE_HOPS: usize = 2;

/// Describe one request with the headers every method row sends, its body if
/// its method has one, and its replay permission if the row writes one.
///
/// The body and the permission are the ones `describe_draft` writes.
fn described<'c>(
    client: &'c Client,
    method: &'static str,
    permission: Permission,
    url: &str,
) -> Result<ClientRequest<'c>, RuntimeError> {
    let request: ClientRequest<'c> = describe_draft(client, Draft::new(method, permission), url)?
        .sensitive_header("authorization", TOKEN)?
        .header("x-tenant", TENANT)?;
    match payload(method) {
        "" => Ok(request),
        _ => request.header("content-type", CONTENT_TYPE),
    }
}

/// Describe one request as [`described`] does, let it follow `hops` same-origin
/// redirects, and send it.
async fn send_described(
    client: &Client,
    method: &'static str,
    permission: Permission,
    hops: usize,
    url: &str,
) -> Result<ClientResponse, RuntimeError> {
    described(client, method, permission, url)?
        .redirect_policy(same_origin(hops))
        .send()
        .await
}

/// What the client does with one redirect answer.
#[derive(Debug, Eq, PartialEq)]
enum Hop {
    /// It refuses, and sends nothing more.
    Refused,
    /// It sends this method with this body to the place the answer names.
    Followed {
        method: &'static str,
        body: &'static str,
    },
}

/// One method row: a status, the method it answers, and the request's replay
/// permission.
#[derive(Clone, Copy, Debug)]
struct TransitionRow {
    status: u16,
    method: &'static str,
    permission: Permission,
}

impl TransitionRow {
    /// The target only this row requests first.
    fn target(self) -> String {
        format!("/hop/{}", self.suffix())
    }

    /// The target only this row's redirect names.
    fn landing(self) -> String {
        format!("/landed/{}", self.suffix())
    }

    fn suffix(self) -> String {
        format!("{}/{:?}/{}", self.status, self.permission, self.method)
    }

    /// Whether the method may be sent again with its body.
    ///
    /// The idempotent methods may, under every permission: `Never` withholds
    /// retries and not an authorized redirect. Any other method needs
    /// `Replayable`.
    fn resendable(self) -> bool {
        matches!(self.permission, Permission::Replayable)
            || Permission::Idempotent.permits(self.method)
    }

    /// The transition the spec's table gives this row.
    fn hop(self) -> Hop {
        let kept = Hop::Followed {
            method: self.method,
            body: payload(self.method),
        };
        match (self.status, self.method) {
            (301 | 302, "GET" | "HEAD") | (303, "HEAD") => kept,
            (301 | 302, _) => Hop::Refused,
            (303, _) => Hop::Followed {
                method: "GET",
                body: "",
            },
            _ if self.resendable() => kept,
            _ => Hop::Refused,
        }
    }
}

/// Every followed status against every method and permission.
fn transition_rows() -> impl Iterator<Item = TransitionRow> {
    FOLLOWED_STATUSES.into_iter().flat_map(|status| {
        PERMISSIONS.into_iter().flat_map(move |permission| {
            METHODS.into_iter().map(move |method| TransitionRow {
                status,
                method,
                permission,
            })
        })
    })
}

/// The method peer's answer: a `/hop/` target redirects with the status its
/// first segment names, to the same path under `/landed/`. Anything else
/// lands. No answer has a body, so every method may receive it.
fn transition_reply(request: &Recorded) -> Vec<u8> {
    let Some(suffix) = request.target().strip_prefix("/hop/") else {
        return answer("200 OK", &[("X-Landed", "yes")], b"");
    };
    let status = suffix.split('/').next().unwrap_or_default();
    let location = format!("/landed/{suffix}");
    answer(
        &format!("{status} Scripted"),
        &[("Location", location.as_str())],
        b"",
    )
}

/// Require one send to carry `method`, `body`, the content type only with a
/// body, and the headers every hop keeps.
fn assert_carried(request: &Recorded, method: &str, body: &str, context: &str) {
    let target = request.target();
    let content_type: &[&str] = match body {
        "" => &[],
        _ => &[CONTENT_TYPE],
    };
    assert_eq!(request.method(), method, "{context}: {target} method");
    assert_eq!(&*request.body, body.as_bytes(), "{context}: {target} body");
    assert_eq!(
        *request.header_values("content-type"),
        *content_type,
        "{context}: {target} content type"
    );
    assert_eq!(
        *request.header_values("authorization"),
        [TOKEN],
        "{context}: {target} credential"
    );
    assert_eq!(
        *request.header_values("x-tenant"),
        [TENANT],
        "{context}: {target} application header"
    );
}

/// Require one method row's answer and its exact sends.
fn assert_transition(
    row: TransitionRow,
    result: Result<ClientResponse, RuntimeError>,
    recording: &Recording,
) {
    let context = format!("{row:?}");
    let first = recording.sent_to(&row.target());
    let landed = recording.sent_to(&row.landing());

    assert_eq!(
        first.len(),
        1,
        "{context}: the first request was sent {} times",
        first.len()
    );
    assert_carried(first[0], row.method, payload(row.method), &context);
    match row.hop() {
        Hop::Refused => {
            assert!(
                matches!(
                    result,
                    Err(RuntimeError::Client(ClientError::Redirect(
                        ClientRedirect::UnsafeMethod
                    )))
                ),
                "{context}: returned {} instead of the unsafe-method refusal",
                describe_result(&result)
            );
            assert!(landed.is_empty(), "{context}: a refused redirect was sent");
        }
        Hop::Followed { method, body } => {
            assert!(
                matches!(&result, Ok(response) if response.status() == 200),
                "{context}: returned {} instead of the landing answer",
                describe_result(&result)
            );
            assert_eq!(
                landed.len(),
                1,
                "{context}: the redirect was sent {} times",
                landed.len()
            );
            assert_carried(landed[0], method, body, &context);
        }
    }
}

/// The whole status, method, and permission table against one peer.
async fn assert_transition_rows() {
    let context = "redirect method transitions";
    let upstream = RecordingUpstream::bind_responder(transition_reply).await;
    let client: Client = http::client().retries(TABLE_RETRIES).backoff(BACKOFF);
    let mut answered = Vec::new();

    for row in transition_rows() {
        let url = upstream.url(&row.target());
        let result: Result<ClientResponse, RuntimeError> = within_bound(
            context,
            send_described(&client, row.method, row.permission, TABLE_HOPS, &url),
        )
        .await;
        answered.push((row, result));
    }
    let recording = upstream.finish(context).await;

    for (row, result) in answered {
        assert_transition(row, result, &recording);
    }
}

/// One scripted answer of the journey peer.
#[derive(Clone, Copy)]
enum Step {
    /// This status line with this `Location`.
    Redirect(&'static str, &'static str),
    /// This status line and no `Location`.
    Status(&'static str),
}

const FOUND: &str = "302 Found";
const UNAVAILABLE: &str = "503 Service Unavailable";
const OK: &str = "200 OK";

/// The answers the journey peer gives each target, one per request for it.
/// The last entry answers every request past the end.
///
/// The alternating scripts end in a 200 no permitted send reaches. A call that
/// sent once too often would return it.
const SCRIPTS: &[(&str, &[Step])] = &[
    (
        "/post/start",
        &[Step::Redirect("303 See Other", "/post/after")],
    ),
    (
        "/post/after",
        &[Step::Status(UNAVAILABLE), Step::Status(OK)],
    ),
    (
        "/never/start",
        &[Step::Redirect("303 See Other", "/never/after")],
    ),
    (
        "/never/after",
        &[Step::Status(UNAVAILABLE), Step::Status(OK)],
    ),
    (
        "/put/start",
        &[Step::Redirect("307 Temporary Redirect", "/put/after")],
    ),
    ("/put/after", &[Step::Status(UNAVAILABLE), Step::Status(OK)]),
    (
        "/patch/start",
        &[Step::Redirect("308 Permanent Redirect", "/patch/after")],
    ),
    (
        "/patch/after",
        &[Step::Status(UNAVAILABLE), Step::Status(OK)],
    ),
    (
        "/spent-retry/a",
        &[
            Step::Status(UNAVAILABLE),
            Step::Redirect(FOUND, "/spent-retry/b"),
        ],
    ),
    (
        "/spent-retry/b",
        &[Step::Status(UNAVAILABLE), Step::Status(OK)],
    ),
    ("/spent-hop/a", &[Step::Redirect(FOUND, "/spent-hop/b")]),
    (
        "/spent-hop/b",
        &[
            Step::Status(UNAVAILABLE),
            Step::Redirect(FOUND, "/spent-hop/c"),
        ],
    ),
    ("/spent-hop/c", &[Step::Status(OK)]),
    (
        "/alternate/redirect-first",
        &[
            Step::Redirect(FOUND, "/alternate/redirect-first"),
            Step::Status(UNAVAILABLE),
            Step::Redirect(FOUND, "/alternate/redirect-first"),
            Step::Status(UNAVAILABLE),
            Step::Redirect(FOUND, "/alternate/redirect-first"),
            Step::Status(OK),
        ],
    ),
    (
        "/alternate/retry-first",
        &[
            Step::Status(UNAVAILABLE),
            Step::Redirect(FOUND, "/alternate/retry-first"),
            Step::Status(UNAVAILABLE),
            Step::Redirect(FOUND, "/alternate/retry-first"),
            Step::Status(UNAVAILABLE),
            Step::Status(OK),
        ],
    ),
    ("/no-retry/a", &[Step::Redirect(FOUND, "/no-retry/b")]),
    (
        "/no-retry/b",
        &[Step::Status(UNAVAILABLE), Step::Status(OK)],
    ),
];

/// The journey peer's answer to request number `index` for `target`.
fn journey_reply(target: &str, index: usize) -> Vec<u8> {
    let step = SCRIPTS
        .iter()
        .find(|(scripted, _)| *scripted == target)
        .and_then(|(_, steps)| steps.get(index).or(steps.last()))
        .copied();
    match step {
        Some(Step::Redirect(status, location)) => answer(status, &[("Location", location)], b""),
        Some(Step::Status(status)) => answer(status, &[], b""),
        None => answer("599 Unscripted", &[], b""),
    }
}

/// A responder that counts the requests it has read for each target and
/// answers each one from [`SCRIPTS`].
fn journey_responder() -> impl Fn(&Recorded) -> Vec<u8> + Send + Sync + 'static {
    let served: Mutex<HashMap<String, usize>> = Mutex::default();
    move |request: &Recorded| {
        let index = {
            let mut served = served.lock().unwrap_or_else(PoisonError::into_inner);
            let count = served.entry(request.target().to_owned()).or_default();
            *count += 1;
            *count - 1
        };
        journey_reply(request.target(), index)
    }
}

/// How one journey ends.
#[derive(Clone, Copy, Debug)]
enum Ending {
    Status(u16),
    Refused(ClientRedirect),
}

/// One call whose redirects and retries share an operation.
#[derive(Clone, Copy, Debug)]
struct Journey {
    label: &'static str,
    method: &'static str,
    permission: Permission,
    retries: u32,
    hops: usize,
    ending: Ending,
    /// The method and target of every send the peer reads, in order. Each
    /// send carries the body [`payload`] gives its method.
    sent: &'static [(&'static str, &'static str)],
}

impl Journey {
    /// A GET under the default permission.
    const fn get(
        label: &'static str,
        retries: u32,
        hops: usize,
        ending: Ending,
        sent: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            label,
            method: "GET",
            permission: Permission::Defaulted,
            retries,
            hops,
            ending,
            sent,
        }
    }

    /// A method with a body, under `permission`, with one retry.
    const fn written(
        label: &'static str,
        method: &'static str,
        permission: Permission,
        ending: Ending,
        sent: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self {
            method,
            permission,
            ..Self::get(label, 1, 4, ending, sent)
        }
    }
}

/// Every journey, in the order it is sent.
const JOURNEYS: &[Journey] = &[
    Journey::written(
        "POST, 303, then a retried GET",
        "POST",
        Permission::Defaulted,
        Ending::Status(200),
        &[
            ("POST", "/post/start"),
            ("GET", "/post/after"),
            ("GET", "/post/after"),
        ],
    ),
    Journey::written(
        "Never follows the 303 and retries nothing",
        "POST",
        Permission::Never,
        Ending::Status(503),
        &[("POST", "/never/start"), ("GET", "/never/after")],
    ),
    Journey::written(
        "PUT, 307, then a retried PUT",
        "PUT",
        Permission::Defaulted,
        Ending::Status(200),
        &[
            ("PUT", "/put/start"),
            ("PUT", "/put/after"),
            ("PUT", "/put/after"),
        ],
    ),
    Journey::written(
        "replayable PATCH, 308, then a retried PATCH",
        "PATCH",
        Permission::Replayable,
        Ending::Status(200),
        &[
            ("PATCH", "/patch/start"),
            ("PATCH", "/patch/after"),
            ("PATCH", "/patch/after"),
        ],
    ),
    Journey::get(
        "a redirect returns no spent retry",
        1,
        4,
        Ending::Status(503),
        &[
            ("GET", "/spent-retry/a"),
            ("GET", "/spent-retry/a"),
            ("GET", "/spent-retry/b"),
        ],
    ),
    Journey::get(
        "a retry returns no spent hop",
        1,
        1,
        Ending::Refused(ClientRedirect::HopLimit),
        &[
            ("GET", "/spent-hop/a"),
            ("GET", "/spent-hop/b"),
            ("GET", "/spent-hop/b"),
        ],
    ),
    Journey::get(
        "both allowances spent, redirect first",
        2,
        2,
        Ending::Refused(ClientRedirect::HopLimit),
        &[("GET", "/alternate/redirect-first"); 5],
    ),
    Journey::get(
        "both allowances spent, retry first",
        2,
        2,
        Ending::Status(503),
        &[("GET", "/alternate/retry-first"); 5],
    ),
    Journey::get(
        "a redirect grants no retry",
        0,
        2,
        Ending::Status(503),
        &[("GET", "/no-retry/a"), ("GET", "/no-retry/b")],
    ),
];

/// Send one journey's first request at the journey peer.
async fn send_journey(
    journey: &Journey,
    upstream: &RecordingUpstream,
) -> Result<ClientResponse, RuntimeError> {
    let client: Client = http::client().retries(journey.retries).backoff(BACKOFF);
    let url = upstream.url(journey.sent[0].1);
    send_described(
        &client,
        journey.method,
        journey.permission,
        journey.hops,
        &url,
    )
    .await
}

/// Require the ending one journey expects.
fn assert_ending(journey: &Journey, result: &Result<ClientResponse, RuntimeError>) {
    let label = journey.label;
    let ended = match (journey.ending, result) {
        (Ending::Status(status), Ok(response)) => response.status() == status,
        (Ending::Refused(kind), Err(RuntimeError::Client(ClientError::Redirect(refused)))) => {
            *refused == kind
        }
        _ => false,
    };
    assert!(
        ended,
        "{label}: returned {} instead of {:?}",
        describe_result(result),
        journey.ending
    );
}

/// Each journey spends one retry for each repeat and one hop for each
/// redirect, and repeats the hop it is on.
///
/// The journeys run in turn against one peer, so the peer's record is every
/// journey's sends in order. One comparison then states each send's method,
/// target, and body, and that no journey sent anything more.
async fn assert_journeys() {
    let context = "redirect and retry journeys";
    let upstream = RecordingUpstream::bind_responder(journey_responder()).await;
    let mut results = Vec::new();

    for journey in JOURNEYS {
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(journey.label, send_journey(journey, &upstream)).await;
        results.push(result);
    }
    let recording = upstream.finish(context).await;

    let read: Vec<(&str, &str, &[u8])> = recording
        .requests
        .iter()
        .map(|request| (request.method(), request.target(), &*request.body))
        .collect();
    let expected: Vec<(&str, &str, &[u8])> = JOURNEYS
        .iter()
        .flat_map(|journey| journey.sent)
        .map(|(method, target)| (*method, *target, payload(method).as_bytes()))
        .collect();
    assert_eq!(read, expected, "{context}: the peer read other sends");
    for request in &recording.requests {
        let body = std::str::from_utf8(&request.body).unwrap();
        assert_carried(request, request.method(), body, context);
    }
    for (journey, result) in JOURNEYS.iter().zip(&results) {
        assert_ending(journey, result);
    }
}

/// A followed redirect whose declared body is never sent.
const HELD_REDIRECT: &[u8] = b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /held/next\r\nContent-Length: 100\r\nConnection: keep-alive\r\n\r\n";

/// A redirect no POST follows, whose declared body is never sent.
const HELD_MOVED: &[u8] = b"HTTP/1.1 301 Moved Permanently\r\nLocation: /held/next\r\nContent-Length: 100\r\nConnection: keep-alive\r\n\r\n";

/// A followed redirect is not drained: the call lands while the redirect's
/// body is still unsent, and the peer sees that connection closed.
async fn assert_held_redirect_is_followed() {
    let context = "followed redirect with an unsent body";
    let mut upstream =
        ScriptedUpstream::bind(&[Answer::Held(HELD_REDIRECT), Answer::Complete]).await;
    let client: Client = http::client().redirect_policy(same_origin(TABLE_HOPS));

    let result: Result<ClientResponse, RuntimeError> =
        within_bound(context, client.get(&upstream.url("/held/start"))).await;
    let started = upstream.saw(PeerEvent::Started(0)).await;
    let continued = upstream
        .saw_each_unordered(&[
            PeerEvent::Disposed(0),
            PeerEvent::Started(1),
            PeerEvent::Answered(1),
        ])
        .await;
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), context).await;

    assert_eq!(started, Ok(()), "{context}: the first send");
    assert_eq!(continued, Ok(()), "{context}: the disposal and the hop");
    assert!(
        matches!(&result, Ok(response) if response.status() == 200 && response.body_bytes() == b"ok"),
        "{context}: returned {}",
        describe_result(&result)
    );
    assert_eq!(starts, 2, "{context}: started {starts} requests");
}

/// A refused redirect is released too, and no retry sends for it again.
///
/// The POST is replayable with retries to spare, so only the refusal stops a
/// second send.
async fn assert_refused_redirect_is_released() {
    let context = "refused redirect with an unsent body";
    let mut upstream = ScriptedUpstream::bind(&[Answer::Held(HELD_MOVED)]).await;
    let client: Client = http::client().retries(TABLE_RETRIES).backoff(BACKOFF);
    let url = upstream.url("/held/start");

    let result: Result<ClientResponse, RuntimeError> = within_bound(
        context,
        send_described(&client, "POST", Permission::Replayable, TABLE_HOPS, &url),
    )
    .await;
    let started = upstream.saw(PeerEvent::Started(0)).await;
    let released = upstream.saw(PeerEvent::Disposed(0)).await;
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), context).await;

    assert_eq!(started, Ok(()), "{context}: the first send");
    assert_released(released, context);
    assert!(
        matches!(
            result,
            Err(RuntimeError::Client(ClientError::Redirect(
                ClientRedirect::UnsafeMethod
            )))
        ),
        "{context}: returned {} instead of the unsafe-method refusal",
        describe_result(&result)
    );
    assert_eq!(starts, 1, "{context}: started {starts} requests");
}

/// Every row that runs on real time.
async fn live_rows() {
    assert_transition_rows().await;
    assert_journeys().await;
    assert_held_redirect_is_followed().await;
    assert_refused_redirect_is_released().await;
}

/// The operation total of the held-clock row.
const TOTAL: Duration = Duration::from_secs(5);

/// A bound that outlasts the held-clock row, for every timer but the total.
const OUTLASTING: Duration = Duration::from_secs(60);

/// The base backoff of the held-clock row.
const HELD_BACKOFF: Duration = Duration::from_millis(10);

/// A redirect's response is released while its hop is still in flight, and an
/// operation past its total starts no hop and no retry.
///
/// The second send's head is held, so the operation cannot have ended when the
/// peer sees the first connection closed. The peer cannot order that close
/// against the second request, which arrives on another socket, so the row
/// takes the three reports in any order. The clock moves onto the total only
/// after them. The call has retries and hops to spare, so a third start after
/// the total would be counted and held.
async fn assert_expired_operation_starts_no_hop() {
    let context = "operation total across a redirect";
    let driver = RunnableDriver::start();
    let mut upstream =
        ScriptedUpstream::bind(&[Answer::Held(HELD_REDIRECT), Answer::StallHead]).await;
    let client: Client = http::client()
        .total_timeout(TOTAL)
        .connect_timeout(OUTLASTING)
        .request_timeout(OUTLASTING)
        .response_idle_timeout(OUTLASTING)
        .retries(TABLE_RETRIES)
        .backoff(HELD_BACKOFF)
        .redirect_policy(same_origin(4));
    let call = spawn_get(client, upstream.url("/held/start"));

    upstream.expect(PeerEvent::Started(0), context).await;
    let continued = upstream
        .saw_each_unordered(&[
            PeerEvent::Disposed(0),
            PeerEvent::Started(1),
            PeerEvent::Stalled(1),
        ])
        .await;
    tokio::time::advance(TOTAL).await;
    let released = upstream.saw(PeerEvent::Released(1)).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    tokio::time::advance(OUTLASTING).await;
    settle_wakeups().await;
    let starts = upstream.finish(driver, context).await;

    assert_eq!(
        continued,
        Ok(()),
        "{context}: the redirect was not released while its hop was in flight"
    );
    assert_released(released, context);
    assert!(
        matches!(
            result,
            Some(Err(RuntimeError::Client(ClientError::Timeout(
                ClientTimeout::Operation
            ))))
        ),
        "{context}: expected the operation timeout, got {}",
        describe(&result)
    );
    assert_eq!(starts, 2, "{context}: the call started {starts} requests");
}

/// Outbound 6.T2: a redirect changes the method and body only as its status
/// says, a retry repeats the hop the call is on, and each send after the
/// first spends one retry or one hop.
///
/// The real-time rows and the held-clock row need different runtimes, so the
/// test owns both and runs them in turn.
#[test]
fn redirect_methods_and_retries_keep_one_effective_request() {
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
    held.block_on(assert_expired_operation_starts_no_hop());
}

/// The text a refused cross-origin policy prints, without its error prefix.
fn policy_refusal(result: Result<RedirectPolicy, RuntimeError>) -> String {
    use camber::http::ClientInput;
    match result {
        Err(RuntimeError::Client(ClientError::InvalidRequest(ClientInput::RedirectPolicy(
            reason,
        )))) => reason.to_string(),
        other => panic!("expected a redirect policy refusal, got {other:?}"),
    }
}

#[test]
fn cross_origin_policy_validates_exact_origins_and_hops() {
    for origin in [
        "",
        "https://user@example.test",
        "https://@example.test",
        "https://:@example.test",
        "https://example.test/path",
        "https://example.test/path/..",
        "https://example.test/path/%2e%2e",
        "https://example.test/.",
        "https://example.test\\",
        "https:\\example.test",
        "https:////example.test",
        "https:/example.test",
        "https:example.test",
        " https://example.test ",
        "https://example.test/?secret",
        "https://example.test/#fragment",
        "file:///tmp",
        "https://exa\nmple.test",
    ] {
        assert_eq!(
            policy_refusal(RedirectPolicy::cross_origin(
                2,
                ["https://valid.example", origin]
            )),
            "origin at index 1",
            "{origin:?}"
        );
    }
    assert_eq!(
        policy_refusal(RedirectPolicy::cross_origin(0, ["https://example.test"])),
        "zero hops"
    );
    assert_eq!(
        policy_refusal(RedirectPolicy::cross_origin(2, Vec::<String>::new())),
        "empty allowlist"
    );
    let refused =
        RedirectPolicy::cross_origin(2, ["https://user:secret-c4a1@example.test"]).unwrap_err();
    assert!(!format!("{refused} {refused:?}").contains("secret-c4a1"));
}

#[test]
fn cross_origin_policy_normalizes_and_reports_its_allowlist() {
    let policy = RedirectPolicy::cross_origin(
        2,
        vec![
            String::from("https://storage.example:8443/"),
            String::from("https://EXAMPLE.test:443/"),
            String::from("https://example.test"),
        ],
    )
    .unwrap();
    assert_eq!(
        policy,
        RedirectPolicy::cross_origin(2, ["https://example.test", "https://storage.example:8443"])
            .unwrap()
    );
    let RedirectPolicy::CrossOrigin(allowance) = &policy else {
        panic!("expected a cross-origin policy, got {policy:?}");
    };
    assert_eq!(allowance.max_hops().get(), 2);
    assert_eq!(
        allowance.origins().collect::<Vec<_>>(),
        ["https://example.test", "https://storage.example:8443"]
    );
    let shown = format!("{policy:?}");
    assert!(shown.contains("https://example.test"), "{shown}");
    assert!(shown.contains("https://storage.example:8443"), "{shown}");
    assert_eq!(
        RedirectPolicy::cross_origin(1, ["https://[::1]:443/"]).unwrap(),
        RedirectPolicy::cross_origin(1, ["https://[0:0:0:0:0:0:0:1]"]).unwrap(),
    );
    assert_ne!(
        RedirectPolicy::cross_origin(1, ["https://example.test"]).unwrap(),
        RedirectPolicy::cross_origin(2, ["https://example.test"]).unwrap(),
    );
}

#[tokio::test]
async fn cross_origin_return_hops_and_retries_never_restore_credentials() {
    for streaming in [false, true] {
        cross_origin_credentials_row(streaming).await;
    }
}

async fn cross_origin_credentials_row(streaming: bool) {
    use crate::retry_upstream::client_answer;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let first_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let second_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let first_origin = format!("http://{}", first_listener.local_addr().unwrap());
    let second_origin = format!("http://{}", second_listener.local_addr().unwrap());
    let return_url = format!("{first_origin}/returned");
    let away_url = format!("{second_origin}/away");
    let first =
        RecordingUpstream::adopt_responder(first_listener, move |request| match request.target() {
            "/start" => answer("307 Temporary Redirect", &[("Location", &away_url)], b""),
            _ => answer("200 OK", &[], b"landed"),
        });
    let attempts = AtomicUsize::new(0);
    let second = RecordingUpstream::adopt_responder(second_listener, move |_| {
        match attempts.fetch_add(1, Ordering::SeqCst) {
            0 => answer(
                "503 Service Unavailable",
                &[("Connection", "close")],
                b"retry",
            ),
            _ => answer("307 Temporary Redirect", &[("Location", &return_url)], b""),
        }
    });
    let policy = RedirectPolicy::cross_origin(2, [&second_origin]).unwrap();
    let client = http::client()
        .retries(1)
        .backoff(BACKOFF)
        .redirect_policy(policy);
    let request = client
        .request("GET", &format!("{first_origin}/start"))
        .unwrap()
        .header("Authorization", TOKEN)
        .unwrap()
        .header("Cookie", "session=credential")
        .unwrap()
        .sensitive_header("x-secret", "first")
        .unwrap()
        .header("x-secret", "overwritten-secret")
        .unwrap()
        .header("x-tenant", TENANT)
        .unwrap();
    let result = within_bound(
        "cross-origin credentials",
        client_answer(request, streaming),
    )
    .await
    .unwrap();
    assert_eq!(result, (200, b"landed".to_vec()));
    let first = first.finish("cross-origin first").await;
    let second = second.finish("cross-origin second").await;
    assert_eq!(*first.targets(), ["/start", "/returned"]);
    assert_eq!(*second.targets(), ["/away", "/away"]);
    assert_eq!(
        first.requests[0].header_values("authorization").as_ref(),
        [TOKEN]
    );
    for request in second.requests.iter().chain(first.requests.iter().skip(1)) {
        for name in ["authorization", "cookie", "x-secret"] {
            assert!(
                request.header_values(name).is_empty(),
                "{name} survived an origin change"
            );
        }
        assert_eq!(request.header_values("x-tenant").as_ref(), [TENANT]);
    }
}

#[tokio::test]
async fn cross_origin_allowlist_and_hop_limit_prevent_extra_sends() {
    use crate::retry_upstream::client_answer;
    for (streaming, allowed) in [(false, false), (false, true), (true, false), (true, true)] {
        let second =
            RecordingUpstream::bind(answer("302 Found", &[("Location", "/again")], b"")).await;
        let destination = second.url("/away");
        let first =
            RecordingUpstream::bind(answer("302 Found", &[("Location", &destination)], b"")).await;
        let origin = second.url("");
        let origins = match allowed {
            true => [origin.as_str()],
            false => ["http://elsewhere.example"],
        };
        let client = http::client()
            .retries(3)
            .redirect_policy(RedirectPolicy::cross_origin(1, origins).unwrap());
        let request = client.request("GET", &first.url("/start")).unwrap();
        let result =
            within_bound("cross-origin allowance", client_answer(request, streaming)).await;
        let expected = match allowed {
            true => ClientRedirect::HopLimit,
            false => ClientRedirect::OriginChanged,
        };
        assert!(
            matches!(result, Err(RuntimeError::Client(ClientError::Redirect(reason))) if reason == expected)
        );
        assert_eq!(
            first
                .finish_probed("cross-origin first")
                .await
                .requests
                .len(),
            1
        );
        assert_eq!(
            second
                .finish_probed("cross-origin denied or spent")
                .await
                .requests
                .len(),
            usize::from(allowed)
        );
    }
}

/// One cross-origin method row: what the first origin answers a request with
/// `method` under `permission`, and whether the second origin receives it.
struct CrossMethod {
    status_line: &'static str,
    method: &'static str,
    permission: camber::http::RetryPolicy,
    /// `true` when the existing replay rule repeats the method and body.
    delivered: bool,
}

const CROSS_METHODS: [CrossMethod; 6] = [
    CrossMethod {
        status_line: "307 Temporary Redirect",
        method: "POST",
        permission: camber::http::RetryPolicy::Idempotent,
        delivered: false,
    },
    CrossMethod {
        status_line: "308 Permanent Redirect",
        method: "POST",
        permission: camber::http::RetryPolicy::Idempotent,
        delivered: false,
    },
    CrossMethod {
        status_line: "307 Temporary Redirect",
        method: "PUT",
        permission: camber::http::RetryPolicy::Idempotent,
        delivered: true,
    },
    CrossMethod {
        status_line: "308 Permanent Redirect",
        method: "PUT",
        permission: camber::http::RetryPolicy::Idempotent,
        delivered: true,
    },
    CrossMethod {
        status_line: "307 Temporary Redirect",
        method: "POST",
        permission: camber::http::RetryPolicy::Replayable,
        delivered: true,
    },
    CrossMethod {
        status_line: "308 Permanent Redirect",
        method: "POST",
        permission: camber::http::RetryPolicy::Replayable,
        delivered: true,
    },
];

/// The body every cross-origin method row sends.
const CROSS_BODY: &[u8] = b"upload-body-7d0e";

/// A 307 or 308 to an allowlisted origin repeats the method and the body only
/// when the request's replay rule lets it. A refused hop dials nothing at the
/// second origin. A delivered one reaches it without credentials.
#[tokio::test]
async fn cross_origin_307_and_308_keep_the_method_and_replay_rules() {
    for streaming in [false, true] {
        for row in &CROSS_METHODS {
            cross_origin_method_row(row, streaming).await;
        }
    }
}

async fn cross_origin_method_row(row: &CrossMethod, streaming: bool) {
    use crate::retry_upstream::client_answer;
    let context = format!(
        "{} {} under {:?}, streaming {streaming}",
        row.status_line, row.method, row.permission
    );
    let second = RecordingUpstream::bind(answer("200 OK", &[], b"stored")).await;
    let destination = second.url("/upload");
    let first =
        RecordingUpstream::bind(answer(row.status_line, &[("Location", &destination)], b"")).await;
    let client =
        http::client().redirect_policy(RedirectPolicy::cross_origin(1, [second.url("")]).unwrap());
    let request = client
        .request(row.method, &first.url("/start"))
        .unwrap()
        .retry_policy(row.permission)
        .header("Authorization", TOKEN)
        .unwrap()
        .sensitive_header("x-api-secret", "secret")
        .unwrap()
        .header("x-tenant", TENANT)
        .unwrap()
        .body(CROSS_BODY);
    let result = within_bound(&context, client_answer(request, streaming)).await;
    let first = first.finish_probed(&context).await;
    let second = second.finish_probed(&context).await;
    assert_eq!(first.requests.len(), 1, "{context}");
    match row.delivered {
        true => {
            assert_eq!(result.unwrap(), (200, b"stored".to_vec()), "{context}");
            let landed = second.only_sent_to(&context, "/upload");
            assert_eq!(landed.method(), row.method, "{context}");
            assert_eq!(&*landed.body, CROSS_BODY, "{context}");
            for name in ["authorization", "x-api-secret"] {
                assert!(
                    landed.header_values(name).is_empty(),
                    "{context}: {name} reached the second origin"
                );
            }
            assert_eq!(landed.header_values("x-tenant").as_ref(), [TENANT]);
        }
        false => {
            assert!(
                matches!(
                    result,
                    Err(RuntimeError::Client(ClientError::Redirect(
                        ClientRedirect::UnsafeMethod
                    )))
                ),
                "{context}: {result:?}"
            );
            assert_eq!(second.connections, 0, "{context}");
        }
    }
}

/// A cross-origin loop spends no hop it cannot count and resets no deadline:
/// with an allowance it cannot exhaust, the operation deadline ends it.
#[tokio::test]
async fn cross_origin_loop_ends_at_the_operation_deadline() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        let first_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let first_origin = format!("http://{}", first_listener.local_addr().unwrap());
        let second_origin = format!("http://{}", second_listener.local_addr().unwrap());
        let to_second = format!("{second_origin}/away");
        let to_first = format!("{first_origin}/back");
        let first = RecordingUpstream::adopt_responder(first_listener, move |_| {
            answer("302 Found", &[("Location", &to_second)], b"")
        });
        let second = RecordingUpstream::adopt_responder(second_listener, move |_| {
            answer("302 Found", &[("Location", &to_first)], b"")
        });
        let policy = RedirectPolicy::cross_origin(usize::MAX, [&second_origin]).unwrap();
        let client = http::client()
            .total_timeout(Duration::from_millis(300))
            .redirect_policy(policy);
        let request = client
            .request("GET", &format!("{first_origin}/start"))
            .unwrap();
        let result = within_bound("cross-origin loop", client_answer(request, streaming)).await;
        assert!(
            matches!(
                result,
                Err(RuntimeError::Client(ClientError::Timeout(
                    ClientTimeout::Operation
                )))
            ),
            "streaming {streaming}: {result:?}"
        );
        let first = first.finish("loop first origin").await;
        let second = second.finish("loop second origin").await;
        assert!(
            first.targets().contains(&"/back"),
            "the loop never returned"
        );
        assert!(!second.requests.is_empty());
    }
}
