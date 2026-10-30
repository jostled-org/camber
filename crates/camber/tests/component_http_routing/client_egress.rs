//! 2.T1, 2.T2, and 2.T3: a configured egress policy governs the connection a
//! buffered request makes, not only the URL it names.
//!
//! Every row sends a public request at real loopback peers. A peer counts the
//! connections it accepted and keeps the requests it read, so a refusal is
//! proven by a peer that accepted nothing, and an answer by the one peer that
//! accepted it.
//!
//! # The controlled edge
//!
//! No local test can change what the system resolver answers. A restricted
//! client therefore takes its lookup answers from a `ClientDnsEdge`, attached
//! with `client_dns_edge(client, &edge)`, which returns the client. Both are
//! doc-hidden and semver-unsupported, and the edge is `Send + Sync`. The
//! edge supplies addresses and counts lookups. It supplies no verdict: every
//! address it returns goes through the production visit bound, the production
//! policy decision, and the production pinned transport. This is the contract
//! the rows read it by:
//!
//! - `ClientDnsEdge::new()` makes an edge with no answers.
//! - `answer(&[IpAddr])` queues the answer of the next unanswered lookup. An
//!   empty slice is a lookup that found no address.
//! - `fail()` queues a lookup that fails.
//! - `hold()` queues a lookup that waits. `held()` resolves when a lookup is
//!   parked there, and `release(&[IpAddr])` answers it. A release whose
//!   operation has ended changes nothing.
//! - `lookups()` is how many lookups the client started.
//! - A lookup with nothing queued fails, so a second lookup in one operation
//!   is visible in the count and in the result.
//!
//! The `system resolver` rows attach no edge. They name `localhost`, the one
//! name every host answers with loopback, so the production lookup runs and
//! its answer is what the policy reads.
//!
//! `client_test_root(client, pem)` adds one local trust root to the transport
//! the production factory builds, and returns `Result<Client, RuntimeError>`.
//! It adds trust. It removes no check.
//!
//! # Clause, row, and the mutation that bounds it
//!
//! | Clause | Row | Mutation the row fails under |
//! |---|---|---|
//! | Default clients reach private peers | `no policy`, `unrestricted policy` | restrict by default |
//! | A literal is checked | `restricted … literal` | skip the check when no lookup runs |
//! | An exception is exact | `exception for another …` | match by suffix, or ignore the port, scheme, or address |
//! | A mixed answer is refused whole | `mixed answer …` | filter the refused candidates and dial the rest |
//! | An empty or failed lookup is `Dns` | `empty answer`, `failed lookup` | read no candidate as no restriction |
//! | 64 visits pass and 65 do not | `64 visits …`, `65 visits …` | truncate, or count after deduplication |
//! | Fallback stays in the snapshot | `unavailable … first`, `63 unavailable visits` | keep one candidate, or drop the last |
//! | A name without an edge is looked up and checked | `system resolver …` | return `Dns` without asking the system resolver |
//! | A new operation resolves again | `a new operation` | reuse the last operation's addresses |
//! | An ended lookup starts nothing | `held lookup …` | let the lookup resume its operation |
//! | One snapshot for one operation | `redirect and retry` | resolve again on a hop or a retry |
//! | No wider pool serves a restricted call | `a warm unrestricted pool` | keep the cached transport |
//! | No implicit proxy | the private child | leave Reqwest's proxy discovery on |
//! | TLS authenticates the name | `trusted certificate for …` | verify the pinned address, or skip a check |
//! | HTTPS-only is read first | `https-only …` | let an exception admit HTTP |
//! | The scheme is read before a mock answers | `https-only, mocked …` | read the scheme only when the network is used |

use crate::error_chain::assert_safe;
use crate::process::ChildEnvironment;
use crate::retry_upstream::{
    Call, Recorded, Recording, RecordingUpstream, RunnableDriver, TIMER_ROUNDING, answer, answered,
    failure, loopback_pair, on_peers, peer_runtime, required, settle_wakeups, settled,
    within_watchdog,
};
use crate::tls_support::generate_cert_with_san;
use crate::untrusted_tls::{NamedTlsEvent, TlsPeerEvent, named_tls_peer};

use camber::__private::ClientDnsEdge;
use camber::__private::client_dns_edge;
use camber::__private::client_test_root;
use camber::__private::client_transport_is_built;
use camber::RuntimeError;
use camber::http;
use camber::http::Client;
use camber::http::ClientEgress;
use camber::http::ClientError;
use camber::http::ClientRedirect;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::EgressPolicy;
use camber::http::RedirectPolicy;
use camber::http::Response;
use camber::http::mock;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::task::JoinError;

/// The name the controlled lookups answer for.
pub(crate) const NAME: &str = "fetch.egress.test";

/// A name no exception of this module admits.
const OTHER_NAME: &str = "other.egress.test";

/// The two loopback addresses the peers listen on.
pub(crate) const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub(crate) const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

/// What every request carries that no error may repeat.
const CREDENTIAL: &str = "credential-sentinel-5e1a";
const PATH_SECRET: &str = "path-sentinel-c2d7";

/// Everything a refusal must not repeat: the sentinels, both names, and the
/// address a refused call would have dialed.
const SECRETS: [&str; 5] = [CREDENTIAL, PATH_SECRET, NAME, OTHER_NAME, "127.0.0.1"];

/// Where a table row writes the port its peers were bound on.
const PORT: &str = "{port}";

/// The origins the rows name, with the peers' port still to be written.
pub(crate) const FETCH: &str = "http://fetch.egress.test:{port}";
pub(crate) const LITERAL_V4: &str = "http://127.0.0.1:{port}";
const LITERAL_V6: &str = "http://[::1]:{port}";

/// The name's origin over HTTPS.
pub(crate) const SECURE_FETCH: &str = "https://fetch.egress.test:{port}";

/// The origin the system resolver answers with loopback, and its `Host`.
const LOCALHOST: &str = "http://localhost:{port}";
const LOCALHOST_HOST: &str = "localhost:{port}";

/// The exceptions of the admitted system-resolver row: `localhost`, at both
/// peers, because a host may answer it with either family or with both.
const LOCALHOST_AT_BOTH: &[(&str, IpAddr)] = &[(LOCALHOST, V4), (LOCALHOST, V6)];

/// The exception most rows hold: the name, at the IPv4 peer.
pub(crate) const FETCH_AT_V4: &[(&str, IpAddr)] = &[(FETCH, V4)];

/// The exception of the HTTPS rows: the name over HTTPS, at the IPv4 peer.
const SECURE_FETCH_AT_V4: &[(&str, IpAddr)] = &[(SECURE_FETCH, V4)];

/// The exceptions of the fallback rows: the name, at both peers.
const FETCH_AT_BOTH: &[(&str, IpAddr)] = &[(FETCH, V4), (FETCH, V6)];

/// The most addresses one operation checks, and one more.
const VISIT_BOUND: usize = 64;
const AT_THE_BOUND: [IpAddr; VISIT_BOUND] = [V4; VISIT_BOUND];
const PAST_THE_BOUND: [IpAddr; VISIT_BOUND + 1] = [V4; VISIT_BOUND + 1];

/// A full answer whose only live address is its last.
const LIVE_LAST: [IpAddr; VISIT_BOUND] = {
    let mut answers = [V6; VISIT_BOUND];
    answers[VISIT_BOUND - 1] = V4;
    answers
};

/// The operation total of the rows that hold a lookup.
const HELD_TOTAL: Duration = Duration::from_secs(5);

/// The base backoff of the rows that cross one retry delay on real time.
pub(crate) const CROSSED_BACKOFF: Duration = Duration::from_millis(1);

pub(crate) const ONE_HOP: NonZeroUsize = NonZeroUsize::new(1).unwrap();

/// `template` with the peers' port written in.
pub(crate) fn at(template: &str, port: u16) -> String {
    template.replace(PORT, &port.to_string())
}

/// A request target at `origin` that carries the path sentinel.
pub(crate) fn target(origin: &str, port: u16) -> String {
    format!("{}/{PATH_SECRET}", at(origin, port))
}

/// `policy` with each exception added in order, written for the peers' port.
fn with_exceptions(
    policy: EgressPolicy,
    row: &str,
    excepted: &[(&str, IpAddr)],
    port: u16,
) -> EgressPolicy {
    excepted
        .iter()
        .try_fold(policy, |policy, (origin, address)| {
            policy.allow_destination(&at(origin, port), *address)
        })
        .unwrap_or_else(|error| panic!("{row}: an exception was refused: {error:?}"))
}

/// A restricted policy with each exception, written for the peers' port.
pub(crate) fn excepting(row: &str, excepted: &[(&str, IpAddr)], port: u16) -> EgressPolicy {
    with_exceptions(EgressPolicy::public_internet(), row, excepted, port)
}

/// A client under `policy` whose lookups are answered by `edge`.
///
/// The policy is set on the client first, so the setter is called on the
/// public type whatever the edge control returns.
pub(crate) fn restricted(policy: EgressPolicy, edge: &ClientDnsEdge) -> Client {
    client_dns_edge(http::client().egress_policy(policy), edge)
}

/// The `Authorization` value that carries the credential sentinel.
fn bearer() -> String {
    format!("Bearer {CREDENTIAL}")
}

/// Send one GET that carries the credential sentinel.
async fn get(client: &Client, url: &str) -> Result<ClientResponse, RuntimeError> {
    client
        .request("GET", url)?
        .sensitive_header("authorization", &bearer())?
        .send()
        .await
}

/// The peer a row expects its answer from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Family {
    V4,
    V6,
}

impl Family {
    /// The body only this family's peer answers with.
    pub(crate) const fn body(self) -> &'static [u8] {
        match self {
            Self::V4 => b"v4",
            Self::V6 => b"v6",
        }
    }

    /// The `Host` template a request to this family's literal carries.
    const fn literal_host(self) -> &'static str {
        match self {
            Self::V4 => "127.0.0.1:{port}",
            Self::V6 => "[::1]:{port}",
        }
    }

    /// The complete answer this family's peer writes.
    fn reply(self) -> Vec<u8> {
        answer("200 OK", &[], self.body())
    }
}

/// One IPv4 and one IPv6 loopback peer on the same port.
///
/// Each answers with its own family, so a response names the address it came
/// from. Both listen in every row that uses the pair, so a refused address is
/// one a dial would have reached.
pub(crate) struct Peers {
    v4: RecordingUpstream,
    v6: RecordingUpstream,
    pub(crate) port: u16,
}

/// Everything both peers read before they were stopped and joined.
pub(crate) struct Seen {
    pub(crate) v4: Recording,
    pub(crate) v6: Recording,
}

impl Peers {
    pub(crate) fn bind() -> Self {
        let (v4, v6) = loopback_pair();
        let port = shared_port(&v4);
        Self {
            v4: RecordingUpstream::adopt_responder(v4, |_| Family::V4.reply()),
            v6: RecordingUpstream::adopt_responder(v6, |_| Family::V6.reply()),
            port,
        }
    }

    /// Probe both peers, stop and join them, and return what each read
    /// before its probe.
    ///
    /// Each peer answers its probe after it accepted every earlier
    /// connection. A dial made before a refusal returned is therefore
    /// counted, and a zero count is complete.
    pub(crate) async fn finish(self, row: &str) -> Seen {
        Seen {
            v4: self.v4.finish_probed(row).await,
            v6: self.v6.finish_probed(row).await,
        }
    }
}

/// The port a bound listener holds.
fn shared_port(listener: &std::net::TcpListener) -> u16 {
    listener
        .local_addr()
        .unwrap_or_else(|error| panic!("a bound listener has no address: {error}"))
        .port()
}

/// Require that neither peer accepted a connection or read a byte before its
/// probe.
pub(crate) fn assert_undialed(row: &str, seen: &Seen) {
    for (family, recording) in [("IPv4", &seen.v4), ("IPv6", &seen.v6)] {
        assert_eq!(
            recording.connections, 0,
            "{row}: the {family} peer accepted a connection"
        );
        assert_eq!(
            recording.starts, 0,
            "{row}: the {family} peer read a request"
        );
    }
}

/// Require a refusal of exactly `category`, safe in every rendering.
fn assert_category(row: &str, result: Result<ClientResponse, RuntimeError>, category: ClientError) {
    let error = failure(row, result);
    assert!(
        matches!(error, RuntimeError::Client(found) if found == category),
        "{row}: returned {error:?} instead of {category:?}"
    );
    assert_safe(&error, &SECRETS, row);
}

/// Require a settled 200 answer whose body is exactly `body`.
///
/// Each peer writes a body of its own, so the body names the answering peer.
fn assert_body(row: &str, result: Option<Result<ClientResponse, RuntimeError>>, body: &[u8]) {
    let response = answered(row, required(row, result));
    assert_eq!(response.status(), 200, "{row}: final status");
    assert_eq!(
        response.body_bytes(),
        body,
        "{row}: the body the answering peer wrote"
    );
}

/// Run `rows` to completion on a current-thread runtime of their own.
fn on_current_thread(rows: impl Future<Output = ()>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime");
    runtime.block_on(rows);
}

/// Which client a row sends through.
#[derive(Clone, Copy, Debug)]
enum Policy {
    /// A client no policy was set on.
    Absent,
    /// A client given the unrestricted policy.
    Unrestricted,
    /// A client restricted to public addresses and the row's exceptions.
    Restricted,
}

/// What the controlled edge answers the row's lookup with.
#[derive(Clone, Copy, Debug)]
enum Lookup {
    /// Nothing is queued, and the row requires that no lookup ran.
    Unasked,
    Answers(&'static [IpAddr]),
    Fails,
}

impl Lookup {
    /// How many lookups one operation of this row starts.
    const fn expected(self) -> usize {
        match self {
            Self::Unasked => 0,
            Self::Answers(_) | Self::Fails => 1,
        }
    }

    fn script(self, edge: &ClientDnsEdge) {
        match self {
            Self::Unasked => {}
            Self::Answers(addresses) => {
                edge.answer(addresses);
            }
            Self::Fails => {
                edge.fail();
            }
        }
    }
}

/// What one row's call must return.
#[derive(Clone, Copy, Debug)]
enum Expected {
    /// An answer from this family's peer, to a request with this `Host`.
    Answered(Family, &'static str),
    Refused(ClientError),
}

/// The refusals the table names.
const ADDRESS: Expected = Expected::Refused(ClientError::Egress(ClientEgress::Address));
const LIMIT: Expected = Expected::Refused(ClientError::Egress(ClientEgress::ResolutionLimit));
const DNS: Expected = Expected::Refused(ClientError::Dns);

/// One buffered GET against a fresh pair of peers.
struct Case {
    row: &'static str,
    policy: Policy,
    /// Each exception: an origin, and the address it is admitted at.
    excepted: &'static [(&'static str, IpAddr)],
    lookup: Lookup,
    /// The origin the request names.
    origin: &'static str,
    expected: Expected,
}

impl Case {
    const fn open(row: &'static str, policy: Policy, origin: &'static str, from: Family) -> Self {
        Self {
            row,
            policy,
            excepted: &[],
            lookup: Lookup::Unasked,
            origin,
            expected: Expected::Answered(from, from.literal_host()),
        }
    }

    /// A restricted call at a literal, which starts no lookup.
    const fn literal(
        row: &'static str,
        excepted: &'static [(&'static str, IpAddr)],
        origin: &'static str,
        expected: Expected,
    ) -> Self {
        Self {
            row,
            policy: Policy::Restricted,
            excepted,
            lookup: Lookup::Unasked,
            origin,
            expected,
        }
    }

    /// A restricted call at a name, answered by the controlled edge.
    const fn named(
        row: &'static str,
        excepted: &'static [(&'static str, IpAddr)],
        lookup: Lookup,
        origin: &'static str,
        expected: Expected,
    ) -> Self {
        Self {
            row,
            policy: Policy::Restricted,
            excepted,
            lookup,
            origin,
            expected,
        }
    }

    fn client(&self, port: u16, edge: &ClientDnsEdge) -> Client {
        match self.policy {
            Policy::Absent => http::client(),
            Policy::Unrestricted => http::client().egress_policy(EgressPolicy::unrestricted()),
            Policy::Restricted => restricted(excepting(self.row, self.excepted, port), edge),
        }
    }
}

/// The `Host` a request for the name carries.
pub(crate) const FETCH_HOST: &str = "fetch.egress.test:{port}";

/// Every table row of 2.T1.
const CASES: &[Case] = &[
    Case::open(
        "no policy, IPv4 literal",
        Policy::Absent,
        LITERAL_V4,
        Family::V4,
    ),
    Case::open(
        "no policy, IPv6 literal",
        Policy::Absent,
        LITERAL_V6,
        Family::V6,
    ),
    Case::open(
        "unrestricted policy, IPv4 literal",
        Policy::Unrestricted,
        LITERAL_V4,
        Family::V4,
    ),
    Case::open(
        "unrestricted policy, IPv6 literal",
        Policy::Unrestricted,
        LITERAL_V6,
        Family::V6,
    ),
    Case::literal("restricted IPv4 literal", &[], LITERAL_V4, ADDRESS),
    Case::literal("restricted IPv6 literal", &[], LITERAL_V6, ADDRESS),
    Case::literal(
        "restricted mapped literal",
        &[],
        "http://[::ffff:127.0.0.1]:{port}",
        ADDRESS,
    ),
    Case::literal(
        "restricted numeric literal",
        &[],
        "http://2130706433:{port}",
        ADDRESS,
    ),
    Case::literal(
        "excepted IPv4 literal",
        &[(LITERAL_V4, V4)],
        LITERAL_V4,
        Expected::Answered(Family::V4, Family::V4.literal_host()),
    ),
    Case::literal(
        "excepted IPv6 literal",
        &[(LITERAL_V6, V6)],
        LITERAL_V6,
        Expected::Answered(Family::V6, Family::V6.literal_host()),
    ),
    Case::literal(
        "exception for the other literal",
        &[(LITERAL_V6, V6)],
        LITERAL_V4,
        ADDRESS,
    ),
    Case::named(
        "excepted name at IPv4",
        FETCH_AT_V4,
        Lookup::Answers(&[V4]),
        FETCH,
        Expected::Answered(Family::V4, FETCH_HOST),
    ),
    Case::named(
        "excepted name at IPv6",
        &[(FETCH, V6)],
        Lookup::Answers(&[V6]),
        FETCH,
        Expected::Answered(Family::V6, FETCH_HOST),
    ),
    Case::named(
        "excepted name, written with capitals and a root dot",
        FETCH_AT_V4,
        Lookup::Answers(&[V4]),
        "http://FETCH.Egress.Test.:{port}",
        Expected::Answered(Family::V4, "fetch.egress.test.:{port}"),
    ),
    Case::named(
        "exception for another name",
        FETCH_AT_V4,
        Lookup::Answers(&[V4]),
        "http://other.egress.test:{port}",
        ADDRESS,
    ),
    Case::named(
        "exception for the parent of the name",
        FETCH_AT_V4,
        Lookup::Answers(&[V4]),
        "http://sub.fetch.egress.test:{port}",
        ADDRESS,
    ),
    Case::named(
        "exception for another port",
        &[("http://fetch.egress.test:1", V4)],
        Lookup::Answers(&[V4]),
        FETCH,
        ADDRESS,
    ),
    Case::named(
        "exception for another address",
        FETCH_AT_V4,
        Lookup::Answers(&[V6]),
        FETCH,
        ADDRESS,
    ),
    Case::named(
        "exception for another scheme",
        &[(SECURE_FETCH, V4)],
        Lookup::Answers(&[V4]),
        FETCH,
        ADDRESS,
    ),
    Case::named(
        "mixed answer, permitted first",
        FETCH_AT_V4,
        Lookup::Answers(&[V4, V6]),
        FETCH,
        ADDRESS,
    ),
    Case::named(
        "mixed answer, refused first",
        FETCH_AT_V4,
        Lookup::Answers(&[V6, V4]),
        FETCH,
        ADDRESS,
    ),
    Case::named(
        "empty answer",
        FETCH_AT_V4,
        Lookup::Answers(&[]),
        FETCH,
        DNS,
    ),
    Case::named("failed lookup", FETCH_AT_V4, Lookup::Fails, FETCH, DNS),
    Case::named(
        "64 visits of one address",
        FETCH_AT_V4,
        Lookup::Answers(&AT_THE_BOUND),
        FETCH,
        Expected::Answered(Family::V4, FETCH_HOST),
    ),
    Case::named(
        "65 visits of one address",
        FETCH_AT_V4,
        Lookup::Answers(&PAST_THE_BOUND),
        FETCH,
        LIMIT,
    ),
];

/// Require the one answer `from` gives, on one connection, to a request that
/// kept the `Host` the caller wrote.
fn assert_answered_by(
    row: &str,
    result: Result<ClientResponse, RuntimeError>,
    seen: &Seen,
    from: Family,
    host: &str,
) {
    let response = answered(row, result);
    let (reached, other) = match from {
        Family::V4 => (&seen.v4, &seen.v6),
        Family::V6 => (&seen.v6, &seen.v4),
    };
    assert_eq!(response.status(), 200, "{row}: final status");
    assert_eq!(response.body_bytes(), from.body(), "{row}: answering peer");
    assert_eq!(reached.connections, 1, "{row}: connections accepted");
    assert_eq!(
        other.connections, 0,
        "{row}: the other family's peer accepted a connection"
    );
    let hosts: Box<[&str]> = reached
        .requests
        .iter()
        .flat_map(|request| request.header_values("host").into_vec())
        .collect();
    assert_eq!(*hosts, [host], "{row}: the Host the one request sent");
}

/// Run one table row against peers of its own.
///
/// Each peer is probed, stopped, and joined before anything is asserted. The
/// probe is accepted after every connection the call made, so every count
/// the row reads is complete.
async fn run_case(case: &Case) {
    let row = case.row;
    let peers = Peers::bind();
    let port = peers.port;
    let edge = ClientDnsEdge::new();
    case.lookup.script(&edge);
    let client: Client = case.client(port, &edge);

    let result = within_watchdog(get(&client, &target(case.origin, port))).await;
    let lookups: usize = edge.lookups();
    let seen = peers.finish(row).await;

    let result = required(row, result);
    assert_eq!(
        lookups,
        case.lookup.expected(),
        "{row}: lookups the operation started"
    );
    match case.expected {
        Expected::Answered(from, host) => {
            assert_answered_by(row, result, &seen, from, &at(host, port));
        }
        Expected::Refused(category) => {
            assert_category(row, result, category);
            assert_undialed(row, &seen);
        }
    }
}

/// One approved address has no listener, and the call reaches the other.
struct Fallback {
    row: &'static str,
    /// The family that still has a peer.
    live: Family,
    answers: &'static [IpAddr],
}

const FALLBACKS: [Fallback; 3] = [
    Fallback {
        row: "unavailable IPv6 first",
        live: Family::V4,
        answers: &[V6, V4],
    },
    Fallback {
        row: "unavailable IPv4 first",
        live: Family::V6,
        answers: &[V4, V6],
    },
    Fallback {
        row: "63 unavailable visits, then the live peer",
        live: Family::V4,
        answers: &LIVE_LAST,
    },
];

/// Both addresses are approved. One half of the pair is dropped, so its port
/// refuses, and the call must reach the half that stayed.
async fn fallback_stays_inside_the_snapshot(fallback: &Fallback) {
    let row = fallback.row;
    let (v4, v6) = loopback_pair();
    let port = shared_port(&v4);
    let (kept, freed) = match fallback.live {
        Family::V4 => (v4, v6),
        Family::V6 => (v6, v4),
    };
    drop(freed);
    let live = fallback.live;
    let peer = RecordingUpstream::adopt_responder(kept, move |_| live.reply());
    let edge = ClientDnsEdge::new();
    edge.answer(fallback.answers);
    let client: Client = restricted(excepting(row, FETCH_AT_BOTH, port), &edge);

    let result = within_watchdog(get(&client, &target(FETCH, port))).await;
    let lookups: usize = edge.lookups();
    let recording = peer.finish(row).await;

    assert_body(row, result, live.body());
    assert_eq!(recording.connections, 1, "{row}: connections accepted");
    assert_eq!(recording.requests.len(), 1, "{row}: requests read");
    assert_eq!(lookups, 1, "{row}: lookups the operation started");
}

/// A name that resolved to a permitted address is checked again by the next
/// operation, which is refused when the answer has changed.
async fn a_new_operation_resolves_and_checks_again() {
    let row = "a new operation";
    let peers = Peers::bind();
    let port = peers.port;
    let edge = ClientDnsEdge::new();
    let client: Client = restricted(excepting(row, FETCH_AT_V4, port), &edge);
    let url = target(FETCH, port);

    edge.answer(&[V4]);
    let first = within_watchdog(get(&client, &url)).await;
    edge.answer(&[V6]);
    let second = within_watchdog(get(&client, &url)).await;
    let lookups: usize = edge.lookups();
    let seen = peers.finish(row).await;

    let first = answered(row, required(row, first));
    assert_eq!(first.body_bytes(), Family::V4.body(), "{row}: first answer");
    assert_category(
        row,
        required(row, second),
        ClientError::Egress(ClientEgress::Address),
    );
    assert_eq!(lookups, 2, "{row}: each operation resolves once");
    assert_eq!(
        (seen.v4.connections, seen.v4.requests.len()),
        (1, 1),
        "{row}: the refused operation reused the first one's connection"
    );
    assert_eq!(
        seen.v6.connections, 0,
        "{row}: the changed answer was dialed"
    );
}

/// The family whose peer wrote `response`.
fn answering_family(row: &str, response: &ClientResponse) -> Family {
    [Family::V4, Family::V6]
        .into_iter()
        .find(|family| response.body_bytes() == family.body())
        .unwrap_or_else(|| panic!("{row}: no peer of the pair wrote the answer"))
}

/// A restricted client with no edge asks the system resolver, and the policy
/// reads that answer: loopback is refused as an address, not as a failed
/// lookup, and neither peer is dialed.
async fn system_resolver_answer_is_refused() {
    let row = "system resolver, localhost without an exception";
    let peers = Peers::bind();
    let port = peers.port;
    let client: Client = http::client().egress_policy(EgressPolicy::public_internet());

    let result = within_watchdog(get(&client, &target(LOCALHOST, port))).await;
    let seen = peers.finish(row).await;

    assert_category(
        row,
        required(row, result),
        ClientError::Egress(ClientEgress::Address),
    );
    assert_undialed(row, &seen);
}

/// The same lookup, with `localhost` excepted at both loopback addresses, is
/// sent to one of the two peers under the name the caller wrote.
async fn system_resolver_answer_is_admitted() {
    let row = "system resolver, localhost excepted at both addresses";
    let peers = Peers::bind();
    let port = peers.port;
    let policy = excepting(row, LOCALHOST_AT_BOTH, port);
    let client: Client = http::client().egress_policy(policy);

    let result = within_watchdog(get(&client, &target(LOCALHOST, port))).await;
    let seen = peers.finish(row).await;

    let response = answered(row, required(row, result));
    let from = answering_family(row, &response);
    assert_answered_by(row, Ok(response), &seen, from, &at(LOCALHOST_HOST, port));
}

/// What ends an operation whose lookup is held.
#[derive(Clone, Copy, Debug)]
enum Ended {
    /// The operation total expires.
    Deadline,
    /// The caller drops the call.
    Dropped,
}

/// How a held call ended, captured without asserting it.
enum HeldEnd {
    Returned(Option<Result<ClientResponse, RuntimeError>>),
    /// What the dropped call's task joined as. `None` is a join the fixture
    /// watchdog ended.
    Dropped(Option<Result<Result<ClientResponse, RuntimeError>, JoinError>>),
}

/// End a call that is parked at its lookup.
async fn end_held(call: Call, ended: Ended) -> HeldEnd {
    match ended {
        Ended::Deadline => {
            tokio::time::advance(HELD_TOTAL + TIMER_ROUNDING).await;
            HeldEnd::Returned(settled(call).await)
        }
        Ended::Dropped => {
            call.abort();
            HeldEnd::Dropped(within_watchdog(call).await)
        }
    }
}

/// A lookup still waiting when its operation ends starts no request, even
/// when it is then answered with a permitted address.
///
/// The clock is paused before the call is first polled. It is advanced only
/// after the edge has acknowledged the parked lookup.
async fn held_lookup_starts_no_request(row: &str, ended: Ended) {
    tokio::time::pause();
    let driver = RunnableDriver::start();
    let peers = Peers::bind();
    let port = peers.port;
    let edge = ClientDnsEdge::new();
    edge.hold();
    let client: Client =
        restricted(excepting(row, FETCH_AT_V4, port), &edge).total_timeout(HELD_TOTAL);
    let url = target(FETCH, port);
    let call: Call = tokio::spawn(async move { get(&client, &url).await });

    let parked = within_watchdog(edge.held()).await;
    let end = end_held(call, ended).await;
    edge.release(&[V4]);
    settle_wakeups().await;
    let lookups: usize = edge.lookups();
    tokio::time::resume();
    driver.stop().await;
    let seen = peers.finish(row).await;

    assert!(parked.is_some(), "{row}: the lookup was never held");
    match (ended, end) {
        (Ended::Deadline, HeldEnd::Returned(result)) => assert_category(
            row,
            required(row, result),
            ClientError::Timeout(ClientTimeout::Operation),
        ),
        (Ended::Dropped, HeldEnd::Dropped(joined)) => {
            assert!(
                matches!(&joined, Some(Err(error)) if error.is_cancelled()),
                "{row}: the dropped call joined as {joined:?}, not as cancelled"
            );
        }
        (Ended::Deadline, HeldEnd::Dropped(_)) | (Ended::Dropped, HeldEnd::Returned(_)) => {
            panic!("{row}: the fixture ended the call another way")
        }
    }
    assert_eq!(lookups, 1, "{row}: lookups the operation started");
    assert_undialed(row, &seen);
}

/// 2.T1
#[tokio::test]
async fn restricted_buffered_dials_only_checked_candidates() {
    for case in CASES {
        run_case(case).await;
    }
    for fallback in &FALLBACKS {
        fallback_stays_inside_the_snapshot(fallback).await;
    }
    a_new_operation_resolves_and_checks_again().await;
    system_resolver_answer_is_refused().await;
    system_resolver_answer_is_admitted().await;
    // Last, because these are the rows that hold the clock still.
    held_lookup_starts_no_request("held lookup, call dropped", Ended::Dropped).await;
    held_lookup_starts_no_request("held lookup, total expired", Ended::Deadline).await;
}

/// The targets the journey peer scripts.
pub(crate) const START: &str = "/start";
pub(crate) const FLAKY: &str = "/flaky";
const AWAY: &str = "/away";
const ANSWER: &str = "/answer";

/// The body of the journey peer's final answer, and of every policy peer.
pub(crate) const LANDED: &[u8] = b"landed";

/// A `Location` on another name, at the port the request arrived on.
fn away_location(request: &Recorded) -> String {
    let port = request
        .header_values("host")
        .first()
        .copied()
        .and_then(|host| host.rsplit(':').next())
        .unwrap_or_default();
    format!("http://{OTHER_NAME}:{port}{ANSWER}")
}

/// Answer one request of the journey by its target.
///
/// `START` redirects to `FLAKY` at the same origin. The first `FLAKY` request
/// is a transient answer that closes its connection, so its retry needs a
/// second dial. `AWAY` redirects to another name.
fn journey(flaky_requests: &AtomicUsize, request: &Recorded) -> Vec<u8> {
    match request.target() {
        START => answer("302 Found", &[("Location", FLAKY)], b""),
        FLAKY if flaky_requests.fetch_add(1, Ordering::SeqCst) == 0 => {
            answer("503 Service Unavailable", &[("Connection", "close")], b"")
        }
        FLAKY | ANSWER => answer("200 OK", &[], LANDED),
        AWAY => answer("302 Found", &[("Location", &away_location(request))], b""),
        _ => answer("500 Internal Server Error", &[], b"unscripted target"),
    }
}

/// A request for `path` at the name, on `port`.
fn journey_url(port: u16, path: &str) -> String {
    format!("{}{path}", at(FETCH, port))
}

/// A request for the answer at the IPv4 literal, on `port`.
fn literal_url(port: u16) -> String {
    format!("{}{ANSWER}", at(LITERAL_V4, port))
}

/// Require a 200 answer with the journey's final body.
fn assert_landed(row: &str, result: Option<Result<ClientResponse, RuntimeError>>) {
    assert_body(row, result, LANDED);
}

/// Require that every request the journey peer read kept the caller's `Host`
/// and credential, and that each operation opened a connection of its own.
fn assert_journey_requests(row: &str, recording: &Recording, port: u16) {
    let targets: Box<[&str]> = recording.targets();
    assert_eq!(
        *targets,
        [START, FLAKY, FLAKY, ANSWER, AWAY],
        "{row}: the requests the peer read"
    );
    let host = at(FETCH_HOST, port);
    let credential = bearer();
    for request in &recording.requests {
        assert_eq!(
            *request.header_values("host"),
            [host.as_str()],
            "{row}: the Host of {}",
            request.target()
        );
        assert_eq!(
            *request.header_values("authorization"),
            [credential.as_str()],
            "{row}: the credential of {}",
            request.target()
        );
    }
    let connections: Box<[usize]> = recording.connections();
    let first_operation = connections[..3].iter().copied().max().unwrap_or_default();
    assert!(
        connections[2] > connections[1],
        "{row}: the retry after a closed connection needs a second dial: {connections:?}"
    );
    assert!(
        connections[3] > first_operation && connections[4] > connections[3],
        "{row}: an operation was served by an earlier operation's pool: {connections:?}"
    );
}

/// A same-origin redirect and a retry are sent under the one snapshot their
/// operation resolved, and the next operation resolves a snapshot of its own.
///
/// The edge is given one answer for each operation. A second lookup inside an
/// operation would find nothing queued and fail the call.
async fn redirect_and_retry_use_one_snapshot() {
    let row = "redirect and retry";
    let flaky_requests = Arc::new(AtomicUsize::new(0));
    let peer =
        RecordingUpstream::bind_responder(move |request| journey(&flaky_requests, request)).await;
    let port = peer.addr().port();
    let edge = ClientDnsEdge::new();
    let client: Client = restricted(excepting(row, FETCH_AT_V4, port), &edge)
        .retries(1)
        .backoff(CROSSED_BACKOFF)
        .redirect_policy(RedirectPolicy::SameOrigin { max_hops: ONE_HOP });

    edge.answer(&[V4]);
    let journeyed = within_watchdog(get(&client, &journey_url(port, START))).await;
    let after_journey: usize = edge.lookups();
    edge.answer(&[V4]);
    let next = within_watchdog(get(&client, &journey_url(port, ANSWER))).await;
    edge.answer(&[V4]);
    let away = within_watchdog(get(&client, &journey_url(port, AWAY))).await;
    let lookups: usize = edge.lookups();
    let cached: bool = client_transport_is_built(&client);
    let recording = peer.finish(row).await;

    assert_landed(row, journeyed);
    assert_eq!(
        after_journey, 1,
        "{row}: three sends of one operation share one lookup"
    );
    assert_landed(row, next);
    assert_category(
        row,
        required(row, away),
        ClientError::Redirect(ClientRedirect::OriginChanged),
    );
    assert_eq!(lookups, 3, "{row}: each operation resolves once");
    assert_journey_requests(row, &recording, port);
    assert!(
        !cached,
        "{row}: a restricted transport was kept in the reusable cache"
    );
}

/// A client that already pooled a connection to a private peer is given a
/// policy. Its next call is refused, and an excepted call after that opens a
/// connection of its own.
async fn a_warm_unrestricted_pool_serves_no_restricted_call() {
    let row = "a warm unrestricted pool";
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let port = peer.addr().port();
    let url = literal_url(port);

    let warm = http::client();
    let warmed = within_watchdog(get(&warm, &url)).await;
    let was_built = client_transport_is_built(&warm);
    let closed: Client = warm.egress_policy(EgressPolicy::public_internet());
    let kept: bool = client_transport_is_built(&closed);
    let refused = within_watchdog(get(&closed, &url)).await;
    let excepted: Client = closed.egress_policy(excepting(row, &[(LITERAL_V4, V4)], port));
    let admitted = within_watchdog(get(&excepted, &url)).await;
    let recording = peer.finish(row).await;

    assert_landed(row, warmed);
    assert!(was_built, "{row}: the first call built no transport");
    assert!(!kept, "{row}: a policy write kept the built transport");
    assert_category(
        row,
        required(row, refused),
        ClientError::Egress(ClientEgress::Address),
    );
    assert_landed(row, admitted);
    let connections: Box<[usize]> = recording.connections();
    assert_eq!(
        *connections,
        [0, 1],
        "{row}: the excepted call was served by the unrestricted pool"
    );
    assert_eq!(recording.connections, 2, "{row}: connections accepted");
}

/// Three clients send at one peer. Each keeps its own policy, its own lookups,
/// and its own pool, and the unrestricted one still reuses its connection.
async fn sibling_clients_keep_their_own_policy() {
    let row = "sibling clients";
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let port = peer.addr().port();
    let literal = literal_url(port);
    let named = journey_url(port, ANSWER);
    let open = http::client();
    let closed_edge = ClientDnsEdge::new();
    let closed: Client = restricted(EgressPolicy::public_internet(), &closed_edge);
    let excepted_edge = ClientDnsEdge::new();
    let excepted: Client = restricted(excepting(row, FETCH_AT_V4, port), &excepted_edge);
    closed_edge.answer(&[V4]);
    excepted_edge.answer(&[V4]);

    let opened = within_watchdog(get(&open, &literal)).await;
    let refused_name = within_watchdog(get(&closed, &named)).await;
    let admitted = within_watchdog(get(&excepted, &named)).await;
    let reopened = within_watchdog(get(&open, &literal)).await;
    let refused_literal = within_watchdog(get(&closed, &literal)).await;
    let lookups: (usize, usize) = (closed_edge.lookups(), excepted_edge.lookups());
    let recording = peer.finish(row).await;

    for result in [opened, admitted, reopened] {
        assert_landed(row, result);
    }
    for result in [refused_name, refused_literal] {
        assert_category(
            row,
            required(row, result),
            ClientError::Egress(ClientEgress::Address),
        );
    }
    assert_eq!(
        lookups,
        (1, 1),
        "{row}: each client resolves through its own edge"
    );
    let connections: Box<[usize]> = recording.connections();
    assert_eq!(
        *connections,
        [0, 1, 0],
        "{row}: the unrestricted client reuses its pool, and no sibling shares it"
    );
}

/// The 2.T2 rows that need no private process.
fn pool_rows() {
    on_current_thread(async {
        redirect_and_retry_use_one_snapshot().await;
        a_warm_unrestricted_pool_serves_no_restricted_call().await;
        sibling_clients_keep_their_own_policy().await;
    });
}

/// The private child of the proxy rows, and the line it ends with.
const PROXY_MODE: &str = "restricted-egress-proxy-environment";
const PROXY_MARKER: &str = "RESTRICTED_EGRESS_PROXY_ROWS_COMPLETE";

/// The name libtest runs this module's 2.T2 test under.
const PROXY_TEST: &str =
    "client_egress::restricted_pool_redirects_retries_and_proxies_use_one_snapshot";

/// How long the private child may take. Every wait inside it is bounded by
/// the fixture watchdog, and none waits for it on a healthy run.
const PROXY_CHILD_BOUND: Duration = Duration::from_secs(120);

/// Every variable Reqwest reads a proxy from. Each one names the trap.
const PROXY_VARIABLES: [&str; 6] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// Every variable that would exempt a target from those proxies, and the one
/// that makes Reqwest ignore `HTTP_PROXY`.
const CLEARED_VARIABLES: [&str; 3] = ["NO_PROXY", "no_proxy", "REQUEST_METHOD"];

/// The name only the positive control asks the trap for.
const CONTROL_NAME: &str = "positive-control.egress.test";

/// What the trap and the direct peers answer with.
const TRAP_BODY: &[u8] = b"trap";
const DIRECT_BODY: &[u8] = b"direct";

/// Answer a forwarded request as the trap, and refuse to tunnel.
///
/// A refused `CONNECT` ends the control's HTTPS call at once. The request is
/// recorded either way.
fn trap_answer(request: &Recorded) -> Vec<u8> {
    match request.method() {
        "CONNECT" => answer("403 Forbidden", &[], b""),
        _ => answer("200 OK", &[], TRAP_BODY),
    }
}

/// An ordinary Reqwest client in this process is sent through the trap.
///
/// Without this row, a trap that nothing can reach would pass every restricted
/// row below it.
async fn ordinary_client_reaches_the_trap() {
    let row = "positive proxy control";
    let ordinary = reqwest::Client::builder()
        .build()
        .unwrap_or_else(|error| panic!("{row}: no ordinary client: {error}"));
    let plain = within_watchdog(
        ordinary
            .get(format!("http://{CONTROL_NAME}/control"))
            .send(),
    )
    .await;
    let tunnelled = within_watchdog(
        ordinary
            .get(format!("https://{CONTROL_NAME}/control"))
            .send(),
    )
    .await;

    let forwarded = match plain {
        Some(Ok(forwarded)) => forwarded,
        other => panic!("{row}: the HTTP control never reached the trap: {other:?}"),
    };
    let body = within_watchdog(forwarded.bytes()).await;
    assert!(
        matches!(&body, Some(Ok(body)) if &body[..] == TRAP_BODY),
        "{row}: the HTTP control was not answered by the trap: {body:?}"
    );
    assert!(
        matches!(tunnelled, Some(Err(_))),
        "{row}: the HTTPS control was not refused by the trap: {tunnelled:?}"
    );
}

/// A complete answer that closes its connection.
pub(crate) fn closing_answer(body: &[u8]) -> Vec<u8> {
    answer("200 OK", &[("Connection", "close")], body)
}

/// The policy of the child's restricted client: the name over HTTP and over
/// HTTPS, and the IPv4 literal, each at its own peer.
fn direct_policy(row: &str, plain_port: u16, secure_port: u16) -> EgressPolicy {
    let named = excepting(row, FETCH_AT_V4, plain_port);
    let secured = with_exceptions(named, row, SECURE_FETCH_AT_V4, secure_port);
    with_exceptions(secured, row, &[(LITERAL_V4, V4)], plain_port)
}

/// Require a 200 answer from a direct peer, not from the trap.
fn assert_direct(row: &str, result: Option<Result<ClientResponse, RuntimeError>>) {
    assert_body(row, result, DIRECT_BODY);
}

/// With every proxy variable naming the trap, a restricted call over HTTP,
/// to a literal, and over HTTPS reaches its own peer directly.
async fn restricted_calls_go_direct() {
    let row = "restricted calls under a proxy environment";
    let plain = RecordingUpstream::bind(answer("200 OK", &[], DIRECT_BODY)).await;
    let (cert_pem, key_pem) = generate_cert_with_san(NAME);
    let mut secure = named_tls_peer(&cert_pem, &key_pem, closing_answer(DIRECT_BODY)).await;
    let (plain_port, secure_port) = (plain.addr().port(), secure.addr().port());
    let edge = ClientDnsEdge::new();
    let policy = direct_policy(row, plain_port, secure_port);
    let client: Client = client_test_root(restricted(policy, &edge), &cert_pem)
        .unwrap_or_else(|error| panic!("{row}: the test root was refused: {error:?}"));

    edge.answer(&[V4]);
    let named = within_watchdog(get(&client, &journey_url(plain_port, ANSWER))).await;
    let literal = within_watchdog(get(&client, &literal_url(plain_port))).await;
    edge.answer(&[V4]);
    let secure_url = format!("{}{ANSWER}", at(SECURE_FETCH, secure_port));
    let secured = within_watchdog(get(&client, &secure_url)).await;
    let handshake = within_watchdog(secure.recv()).await;
    secure.finish(row, within_watchdog).await;
    let recording = plain.finish(row).await;

    for result in [named, literal, secured] {
        assert_direct(row, result);
    }
    let targets: Box<[&str]> = recording.targets();
    assert_eq!(
        *targets,
        [ANSWER, ANSWER],
        "{row}: the plain peer reads origin-form requests, sent to it directly"
    );
    assert_named_request(row, &handshake, secure_port);
}

/// The rows the private child runs. Its environment already names the trap.
fn proxy_rows() {
    on_current_thread(async {
        ordinary_client_reaches_the_trap().await;
        restricted_calls_go_direct().await;
    });
}

/// Require that the trap accepted the two control connections, read their two
/// requests, and nothing else.
fn assert_only_the_control_was_trapped(trapped: &Recording) {
    let row = "the proxy trap";
    let mut lines: Vec<String> = trapped
        .requests
        .iter()
        .map(|request| format!("{} {}", request.method(), request.target()))
        .collect();
    lines.sort();
    assert_eq!(
        lines,
        [
            format!("CONNECT {CONTROL_NAME}:443"),
            format!("GET http://{CONTROL_NAME}/control"),
        ],
        "{row}: the requests the trap read"
    );
    assert_eq!(trapped.starts, 2, "{row}: request starts the trap saw");
    // A restricted dial that reached the trap and wrote nothing is a
    // connection with no request. Only the count of accepts shows it.
    assert_eq!(
        trapped.connections, 2,
        "{row}: connections the trap accepted"
    );
    for request in &trapped.requests {
        assert!(
            !request.head.contains(NAME) && !request.head.contains(CREDENTIAL),
            "{row}: a restricted request reached the trap: {}",
            request.head
        );
    }
}

/// Run the pool rows here, then the proxy rows in a child whose environment
/// names a trap this process owns.
///
/// The trap runs on a runtime of its own, because this thread blocks on the
/// child. It is stopped and joined after the child has exited. The trap is
/// probed first, so its accept count holds every dial the child made.
fn drive_proxy_child() {
    pool_rows();
    let peers = peer_runtime();
    let trap = on_peers(&peers, RecordingUpstream::bind_responder(trap_answer));
    let proxy = format!("http://{}", trap.addr());
    let set: Box<[(&str, &str)]> = PROXY_VARIABLES
        .iter()
        .map(|name| (*name, proxy.as_str()))
        .collect();
    let environment = ChildEnvironment {
        set: &set,
        removed: &CLEARED_VARIABLES,
    };

    crate::process::run_in_child_in(
        PROXY_TEST,
        PROXY_MODE,
        PROXY_MARKER,
        PROXY_CHILD_BOUND,
        &environment,
        proxy_rows,
    );
    let trapped = on_peers(&peers, trap.finish_probed("the proxy trap"));

    assert_only_the_control_was_trapped(&trapped);
}

/// 2.T2
#[test]
fn restricted_pool_redirects_retries_and_proxies_use_one_snapshot() {
    match crate::process::is_private_child(PROXY_MODE) {
        true => {
            crate::process::run_in_child(
                PROXY_TEST,
                PROXY_MODE,
                PROXY_MARKER,
                PROXY_CHILD_BOUND,
                proxy_rows,
            );
        }
        false => drive_proxy_child(),
    }
}

/// The body of a TLS peer's answer.
pub(crate) const SECURE_BODY: &[u8] = b"secure";

/// Which certificate a TLS row's client trusts.
#[derive(Clone, Copy, Debug)]
enum Trust {
    /// The certificate the peer presents.
    Presented,
    /// Another certificate for the requested name, which did not issue the
    /// peer's.
    Stranger,
}

/// One HTTPS request for the name, pinned to a TLS peer on loopback.
struct TlsCase {
    row: &'static str,
    /// The one name the peer's certificate holds.
    presented: &'static str,
    trust: Trust,
    /// Whether the handshake must complete.
    trusted: bool,
}

const TLS_CASES: [TlsCase; 4] = [
    TlsCase {
        row: "trusted certificate for the requested name",
        presented: NAME,
        trust: Trust::Presented,
        trusted: true,
    },
    TlsCase {
        row: "trusted certificate for another name",
        presented: OTHER_NAME,
        trust: Trust::Presented,
        trusted: false,
    },
    TlsCase {
        row: "trusted certificate for the pinned address",
        presented: "127.0.0.1",
        trust: Trust::Presented,
        trusted: false,
    },
    TlsCase {
        row: "requested name from an untrusted issuer",
        presented: NAME,
        trust: Trust::Stranger,
        trusted: false,
    },
];

/// An HTTPS-only policy that admits the name at the IPv4 peer.
pub(crate) fn secure_policy(row: &str, port: u16) -> EgressPolicy {
    let https_only = EgressPolicy::public_internet().https_only();
    with_exceptions(https_only, row, SECURE_FETCH_AT_V4, port)
}

/// Require a completed handshake for the name, and a request that carried the
/// name as its `Host` and the credential.
fn assert_named_request(row: &str, handshake: &Option<Option<NamedTlsEvent>>, port: u16) {
    let head = String::from_utf8_lossy(named_request(row, handshake));
    let hosts: Box<[&str]> = crate::http::header_values(&head, "host").collect();
    assert_eq!(
        *hosts,
        [at(FETCH_HOST, port).as_str()],
        "{row}: the Host the request sent"
    );
    assert!(
        head.contains(CREDENTIAL),
        "{row}: the credential did not travel, so its absence elsewhere proves nothing"
    );
}

/// Require a handshake the client refused after it asked for the name.
///
/// A refused handshake carries no application data, so the credential was
/// never sent.
pub(crate) fn assert_refused_handshake(row: &str, handshake: Option<Option<NamedTlsEvent>>) {
    assert_eq!(
        handshake,
        Some(Some(NamedTlsEvent {
            server_name: Some(NAME.into()),
            reached: TlsPeerEvent::HandshakeRefused,
        })),
        "{row}: the client must ask for the name, then refuse the certificate"
    );
}

/// Require a completed handshake that asked for the name, and return the
/// application bytes the peer read.
pub(crate) fn named_request<'a>(
    row: &str,
    handshake: &'a Option<Option<NamedTlsEvent>>,
) -> &'a [u8] {
    match handshake {
        Some(Some(NamedTlsEvent {
            server_name: Some(server_name),
            reached: TlsPeerEvent::Completed(sent),
        })) => {
            assert_eq!(&**server_name, NAME, "{row}: the server name asked for");
            sent
        }
        other => panic!("{row}: the TLS peer saw {other:?}"),
    }
}

/// Require a completed handshake, whatever name it asked for.
pub(crate) fn assert_completed_handshake(row: &str, handshake: &Option<Option<NamedTlsEvent>>) {
    assert!(
        matches!(
            handshake,
            Some(Some(NamedTlsEvent {
                reached: TlsPeerEvent::Completed(_),
                ..
            }))
        ),
        "{row}: the TLS peer saw {handshake:?}"
    );
}

/// Run one TLS row against a peer of its own.
async fn run_tls_case(case: &TlsCase) {
    let row = case.row;
    let (cert_pem, key_pem) = generate_cert_with_san(case.presented);
    let stranger = matches!(case.trust, Trust::Stranger).then(|| generate_cert_with_san(NAME).0);
    let root_pem: &[u8] = stranger.as_deref().unwrap_or(&cert_pem);
    let mut peer = named_tls_peer(&cert_pem, &key_pem, closing_answer(SECURE_BODY)).await;
    let port = peer.addr().port();
    let edge = ClientDnsEdge::new();
    edge.answer(&[V4]);
    let client: Client = client_test_root(restricted(secure_policy(row, port), &edge), root_pem)
        .unwrap_or_else(|error| panic!("{row}: the test root was refused: {error:?}"));

    let result = within_watchdog(get(&client, &target(SECURE_FETCH, port))).await;
    let handshake = within_watchdog(peer.recv()).await;
    let lookups: usize = edge.lookups();
    peer.finish(row, within_watchdog).await;

    let result = required(row, result);
    assert_eq!(lookups, 1, "{row}: lookups the operation started");
    match case.trusted {
        true => {
            assert_body(row, Some(result), SECURE_BODY);
            assert_named_request(row, &handshake, port);
        }
        false => {
            assert_category(row, result, ClientError::Tls);
            assert_refused_handshake(row, handshake);
        }
    }
}

/// An HTTPS-only policy refuses an HTTP target before it resolves or dials,
/// whichever exception was written and in whichever order.
async fn https_only_refuses_http_before_resolution() {
    let peers = Peers::bind();
    let port = peers.port;
    let policies = [
        ("https-only", EgressPolicy::public_internet().https_only()),
        (
            "https-only, written after an HTTP exception",
            excepting("https-only", FETCH_AT_V4, port).https_only(),
        ),
        (
            "https-only, written before an HTTP exception",
            with_exceptions(
                EgressPolicy::public_internet().https_only(),
                "https-only",
                FETCH_AT_V4,
                port,
            ),
        ),
        (
            "https-only, unrestricted",
            EgressPolicy::unrestricted().https_only(),
        ),
    ];
    let mut results = Vec::new();
    for (row, policy) in policies {
        let edge = ClientDnsEdge::new();
        let client: Client = restricted(policy, &edge);
        let named = within_watchdog(get(&client, &target(FETCH, port))).await;
        let literal = within_watchdog(get(&client, &target(LITERAL_V4, port))).await;
        let lookups: usize = edge.lookups();
        results.push((row, named, literal, lookups));
    }
    let seen = peers.finish("https-only").await;

    for (row, named, literal, lookups) in results {
        let scheme = ClientError::Egress(ClientEgress::Scheme);
        assert_category(row, required(row, named), scheme);
        assert_category(row, required(row, literal), scheme);
        assert_eq!(lookups, 0, "{row}: a refused scheme was resolved");
    }
    assert_undialed("https-only", &seen);
}

/// The HTTP target only the mock rows name, and the body its mock answers.
const MOCKED: &str = "http://mocked.egress.test/answer";
const MOCKED_BODY: &str = "mocked";

/// An HTTPS-only policy refuses an HTTP target before a registered mock
/// answers it, and a policy that permits HTTP receives the mock's answer.
///
/// This proves the order of admission. A mock resolves nothing and dials
/// nothing, so it is no evidence of where a connection would go. The mock is
/// asked for its one call after both sends: the refused call is not it.
async fn https_only_refuses_http_before_a_mock_answers() {
    let row = "https-only, mocked HTTP target";
    let control = "HTTP permitted, mocked HTTP target";
    let canned = Response::text(200, MOCKED_BODY)
        .unwrap_or_else(|error| panic!("{row}: the mock answer was refused: {error:?}"));
    let registered = mock::http(MOCKED).returns(canned);
    let closed_edge = ClientDnsEdge::new();
    let closed: Client = restricted(EgressPolicy::public_internet().https_only(), &closed_edge);
    let open_edge = ClientDnsEdge::new();
    let open: Client = restricted(EgressPolicy::public_internet(), &open_edge);

    let refused = within_watchdog(get(&closed, MOCKED)).await;
    let mocked = within_watchdog(get(&open, MOCKED)).await;
    let lookups: (usize, usize) = (closed_edge.lookups(), open_edge.lookups());
    registered.assert_called_once();
    drop(registered);

    assert_category(
        row,
        required(row, refused),
        ClientError::Egress(ClientEgress::Scheme),
    );
    assert_body(control, mocked, MOCKED_BODY.as_bytes());
    assert_eq!(lookups, (0, 0), "{row}: a mocked call was resolved");
}

/// A scheme-only policy adds no address restriction: an HTTPS call to a
/// private literal is sent, and its certificate is still checked.
async fn scheme_only_policy_keeps_private_access() {
    let row = "https-only, unrestricted, private HTTPS literal";
    let (cert_pem, key_pem) = generate_cert_with_san("127.0.0.1");
    let mut peer = named_tls_peer(&cert_pem, &key_pem, closing_answer(SECURE_BODY)).await;
    let port = peer.addr().port();
    let policy = EgressPolicy::unrestricted().https_only();
    let client: Client = client_test_root(http::client().egress_policy(policy), &cert_pem)
        .unwrap_or_else(|error| panic!("{row}: the test root was refused: {error:?}"));

    let result = within_watchdog(get(&client, &format!("https://127.0.0.1:{port}{ANSWER}"))).await;
    let handshake = within_watchdog(peer.recv()).await;
    peer.finish(row, within_watchdog).await;

    assert_body(row, result, SECURE_BODY);
    assert_completed_handshake(row, &handshake);
}

/// 2.T3
#[tokio::test]
async fn restricted_tls_authenticates_the_requested_host() {
    for case in &TLS_CASES {
        run_tls_case(case).await;
    }
    https_only_refuses_http_before_resolution().await;
    https_only_refuses_http_before_a_mock_answers().await;
    scheme_only_policy_keeps_private_access().await;
}

fn cross_origin_client(
    first: &RecordingUpstream,
    second: &RecordingUpstream,
    edge: &ClientDnsEdge,
) -> Client {
    let first_origin = format!("http://first.egress.test:{}", first.addr().port());
    let second_origin = format!("http://second.egress.test:{}", second.addr().port());
    let policy = EgressPolicy::public_internet()
        .allow_destination(&first_origin, V4)
        .unwrap()
        .allow_destination(&second_origin, V4)
        .unwrap();
    restricted(policy, edge)
        .redirect_policy(RedirectPolicy::cross_origin(3, [&second_origin]).unwrap())
        .retries(1)
        .backoff(CROSSED_BACKOFF)
}

fn cross_origin_url(peer: &RecordingUpstream, name: &str) -> String {
    format!("http://{name}.egress.test:{}/start", peer.addr().port())
}

#[tokio::test]
async fn cross_origin_new_destination_rejects_forbidden_and_mixed_dns_answers() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        for addresses in [vec![V6], vec![V4, V6]] {
            let second = RecordingUpstream::bind(answer("200 OK", &[], b"must not arrive")).await;
            let away = cross_origin_url(&second, "second");
            let first =
                RecordingUpstream::bind(answer("302 Found", &[("Location", &away)], b"")).await;
            let edge = ClientDnsEdge::new();
            edge.answer(&[V4]);
            edge.answer(&addresses);
            let client = cross_origin_client(&first, &second, &edge);
            let result = within_watchdog(client_answer(
                client
                    .request("GET", &cross_origin_url(&first, "first"))
                    .unwrap(),
                streaming,
            ))
            .await
            .unwrap();
            assert!(matches!(
                result,
                Err(RuntimeError::Client(ClientError::Egress(
                    ClientEgress::Address
                )))
            ));
            assert_eq!(edge.lookups(), 2);
            assert_eq!(first.finish_probed("first origin").await.requests.len(), 1);
            let refused = second.finish_probed("forbidden second origin").await;
            assert_eq!(refused.connections, 0);
            assert!(refused.requests.is_empty());
        }
    }
}

#[tokio::test]
async fn cross_origin_retry_reuses_the_new_destination_pin() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        let attempts = AtomicUsize::new(0);
        let second = RecordingUpstream::bind_responder(move |_| {
            match attempts.fetch_add(1, Ordering::SeqCst) {
                0 => answer("503 Service Unavailable", &[("Connection", "close")], b""),
                _ => answer("200 OK", &[], b"pinned"),
            }
        })
        .await;
        let away = cross_origin_url(&second, "second");
        let first = RecordingUpstream::bind(answer("302 Found", &[("Location", &away)], b"")).await;
        let edge = ClientDnsEdge::new();
        edge.answer(&[V4]);
        edge.answer(&[V4]);
        let client = cross_origin_client(&first, &second, &edge);
        let result = within_watchdog(client_answer(
            client
                .request("GET", &cross_origin_url(&first, "first"))
                .unwrap(),
            streaming,
        ))
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, (200, b"pinned".to_vec()));
        assert_eq!(edge.lookups(), 2, "the retry must reuse the second pin");
        assert!(!client_transport_is_built(&client));
        assert_eq!(first.finish("initial pin").await.requests.len(), 1);
        let landed = second.finish("second pin").await;
        assert_eq!(landed.requests.len(), 2);
        for request in &landed.requests {
            assert_eq!(
                request.header_values("host").as_ref(),
                [format!(
                    "second.egress.test:{}",
                    reqwest::Url::parse(&away).unwrap().port().unwrap()
                )
                .as_str()]
            );
        }
    }
}

#[tokio::test]
async fn cross_origin_pending_destination_ends_with_its_original_operation() {
    for streaming in [false, true] {
        for deadline in [false, true] {
            cross_origin_held_destination(streaming, deadline).await;
        }
    }
}

async fn cross_origin_held_destination(streaming: bool, deadline: bool) {
    use crate::retry_upstream::client_answer;
    let second = RecordingUpstream::bind(answer("200 OK", &[], b"too late")).await;
    let away = cross_origin_url(&second, "second");
    let first = RecordingUpstream::bind(answer("302 Found", &[("Location", &away)], b"")).await;
    let edge = ClientDnsEdge::new();
    edge.answer(&[V4]);
    edge.hold();
    let client = cross_origin_client(&first, &second, &edge).total_timeout(HELD_TOTAL);
    let url = cross_origin_url(&first, "first");
    tokio::time::pause();
    let driver = RunnableDriver::start();
    let call = tokio::spawn(async move {
        client_answer(client.request("GET", &url).unwrap(), streaming).await
    });
    assert!(within_watchdog(edge.held()).await.is_some());
    match deadline {
        true => {
            tokio::time::advance(HELD_TOTAL + TIMER_ROUNDING).await;
            let result = within_watchdog(call).await.unwrap().unwrap();
            assert!(matches!(
                result,
                Err(RuntimeError::Client(ClientError::Timeout(
                    ClientTimeout::Operation
                )))
            ));
        }
        false => {
            call.abort();
            assert!(
                within_watchdog(call)
                    .await
                    .unwrap()
                    .unwrap_err()
                    .is_cancelled()
            );
        }
    }
    edge.release(&[V4]);
    settle_wakeups().await;
    assert_eq!(edge.lookups(), 2);
    tokio::time::resume();
    driver.stop().await;
    assert_eq!(
        first.finish_probed("initial redirect").await.requests.len(),
        1
    );
    assert_eq!(
        second
            .finish_probed("abandoned destination")
            .await
            .connections,
        0
    );
}

#[tokio::test]
async fn cross_origin_https_downgrade_is_refused_even_when_allowlisted() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        let second = RecordingUpstream::bind(answer("200 OK", &[], b"must stay private")).await;
        let away = second.url("/downgraded");
        let (cert, key) = generate_cert_with_san(NAME);
        let mut first = named_tls_peer(
            &cert,
            &key,
            answer("302 Found", &[("Location", &away)], b""),
        )
        .await;
        let first_origin = format!("https://{NAME}:{}", first.addr().port());
        let edge = ClientDnsEdge::new();
        edge.answer(&[V4]);
        let policy = EgressPolicy::public_internet()
            .allow_destination(&first_origin, V4)
            .unwrap()
            .allow_destination(&second.url(""), V4)
            .unwrap();
        let client = client_test_root(restricted(policy, &edge), &cert)
            .unwrap()
            .redirect_policy(RedirectPolicy::cross_origin(2, [&second.url("")]).unwrap());
        let result = within_watchdog(client_answer(
            client.request("GET", &first_origin).unwrap(),
            streaming,
        ))
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(RuntimeError::Client(ClientError::Redirect(
                ClientRedirect::Downgrade
            )))
        ));
        let event = within_watchdog(first.recv()).await;
        assert_completed_handshake("redirect downgrade", &event);
        first.finish("secure first origin", within_watchdog).await;
        assert_eq!(edge.lookups(), 1);
        assert_eq!(
            second.finish_probed("downgrade target").await.connections,
            0
        );
    }
}

/// HTTPS A to HTTPS B to HTTP C: the second hop leaves HTTPS, so it is
/// refused although C is allowlisted and admitted by egress policy.
#[tokio::test]
async fn cross_origin_https_chain_never_downgrades_on_a_later_hop() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        let third = RecordingUpstream::bind(answer("200 OK", &[], b"must stay private")).await;
        let third_origin = format!("http://{NAME}:{}", third.addr().port());
        let (cert, key) = generate_cert_with_san(NAME);
        let mut second = named_tls_peer(
            &cert,
            &key,
            answer(
                "302 Found",
                &[("Location", &format!("{third_origin}/c"))],
                b"",
            ),
        )
        .await;
        let second_origin = format!("https://{NAME}:{}", second.addr().port());
        let mut first = named_tls_peer(
            &cert,
            &key,
            answer(
                "307 Temporary Redirect",
                &[("Location", &second_origin)],
                b"",
            ),
        )
        .await;
        let first_origin = format!("https://{NAME}:{}", first.addr().port());
        let edge = ClientDnsEdge::new();
        edge.answer(&[V4]);
        edge.answer(&[V4]);
        let policy = EgressPolicy::public_internet()
            .allow_destination(&first_origin, V4)
            .unwrap()
            .allow_destination(&second_origin, V4)
            .unwrap()
            .allow_destination(&third_origin, V4)
            .unwrap();
        let client = client_test_root(restricted(policy, &edge), &cert)
            .unwrap()
            .redirect_policy(
                RedirectPolicy::cross_origin(3, [&second_origin, &third_origin]).unwrap(),
            );
        let result = within_watchdog(client_answer(
            client.request("GET", &first_origin).unwrap(),
            streaming,
        ))
        .await
        .unwrap();
        assert!(
            matches!(
                result,
                Err(RuntimeError::Client(ClientError::Redirect(
                    ClientRedirect::Downgrade
                )))
            ),
            "streaming {streaming}: {result:?}"
        );
        assert_completed_handshake("chain first hop", &within_watchdog(first.recv()).await);
        assert_completed_handshake("chain second hop", &within_watchdog(second.recv()).await);
        first.finish("chain first origin", within_watchdog).await;
        second.finish("chain second origin", within_watchdog).await;
        assert_eq!(edge.lookups(), 2, "the refused HTTP origin was resolved");
        assert_eq!(third.finish_probed("downgrade target").await.connections, 0);
    }
}

#[tokio::test]
async fn cross_origin_return_requires_fresh_dns_admission() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        let first_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let second_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let first_origin = format!(
            "http://first.egress.test:{}",
            first_listener.local_addr().unwrap().port()
        );
        let second_origin = format!(
            "http://second.egress.test:{}",
            second_listener.local_addr().unwrap().port()
        );
        let away = format!("{second_origin}/away");
        let returned = format!("{first_origin}/returned");
        let first = RecordingUpstream::adopt_responder(first_listener, move |_| {
            answer("302 Found", &[("Location", &away)], b"")
        });
        let second = RecordingUpstream::adopt_responder(second_listener, move |_| {
            answer("302 Found", &[("Location", &returned)], b"")
        });
        let edge = ClientDnsEdge::new();
        edge.answer(&[V4]);
        edge.answer(&[V4]);
        edge.answer(&[V6]);
        let client = cross_origin_client(&first, &second, &edge);
        let result = within_watchdog(client_answer(
            client.request("GET", &first_origin).unwrap(),
            streaming,
        ))
        .await
        .unwrap();
        assert!(matches!(
            result,
            Err(RuntimeError::Client(ClientError::Egress(
                ClientEgress::Address
            )))
        ));
        assert_eq!(edge.lookups(), 3, "returning to A must check A again");
        assert_eq!(
            first.finish_probed("return refused").await.requests.len(),
            1
        );
        assert_eq!(
            second.finish_probed("away admitted").await.requests.len(),
            1
        );
    }
}

#[tokio::test]
async fn cross_origin_releases_unread_response_before_destination_dns() {
    use crate::retry_upstream::{Pace, PacedUpstream, client_answer, paced_head};
    for streaming in [false, true] {
        let second = RecordingUpstream::bind(answer("200 OK", &[], b"landed")).await;
        let second_origin = format!("http://second.egress.test:{}", second.addr().port());
        let mut first = PacedUpstream::bind_steps(vec![
            Pace::Send(paced_head(
                "302 Found",
                &[("Location", &second_origin), ("Content-Length", "100")],
            )),
            Pace::UntilClosed,
        ])
        .await;
        let first_origin = format!("http://first.egress.test:{}", first.addr().port());
        let edge = ClientDnsEdge::new();
        edge.answer(&[V4]);
        edge.hold();
        let policy = EgressPolicy::public_internet()
            .allow_destination(&first_origin, V4)
            .unwrap()
            .allow_destination(&second_origin, V4)
            .unwrap();
        let client = restricted(policy, &edge)
            .redirect_policy(RedirectPolicy::cross_origin(1, [&second_origin]).unwrap());
        let call = tokio::spawn(async move {
            client_answer(client.request("GET", &first_origin).unwrap(), streaming).await
        });
        assert!(within_watchdog(edge.held()).await.is_some());
        first
            .must_see_client_left("DNS cannot retain the unread redirect body")
            .await;
        edge.release(&[V4]);
        assert_eq!(
            within_watchdog(call).await.unwrap().unwrap().unwrap(),
            (200, b"landed".to_vec())
        );
        assert_eq!(edge.lookups(), 2);
        first.finish("released redirect").await;
        assert_eq!(second.finish("checked destination").await.requests.len(), 1);
    }
}

#[tokio::test]
async fn cross_origin_retains_tls_trust_at_the_checked_destination() {
    use crate::retry_upstream::client_answer;
    for streaming in [false, true] {
        let (cert, key) = generate_cert_with_san(NAME);
        let mut second = named_tls_peer(&cert, &key, answer("200 OK", &[], b"trusted")).await;
        let second_origin = format!("https://{NAME}:{}", second.addr().port());
        let mut first = named_tls_peer(
            &cert,
            &key,
            answer("302 Found", &[("Location", &second_origin)], b""),
        )
        .await;
        let first_origin = format!("https://{NAME}:{}", first.addr().port());
        let edge = ClientDnsEdge::new();
        edge.answer(&[V4]);
        edge.answer(&[V4]);
        let policy = EgressPolicy::public_internet()
            .allow_destination(&first_origin, V4)
            .unwrap()
            .allow_destination(&second_origin, V4)
            .unwrap();
        let client = client_test_root(restricted(policy, &edge), &cert)
            .unwrap()
            .redirect_policy(RedirectPolicy::cross_origin(1, [&second_origin]).unwrap());
        let result = within_watchdog(client_answer(
            client.request("GET", &first_origin).unwrap(),
            streaming,
        ))
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, (200, b"trusted".to_vec()));
        assert_eq!(edge.lookups(), 2);
        let first_event = within_watchdog(first.recv()).await;
        let second_event = within_watchdog(second.recv()).await;
        assert_completed_handshake("initial TLS origin", &first_event);
        assert_completed_handshake("checked TLS destination", &second_event);
        first.finish("initial TLS origin", within_watchdog).await;
        second
            .finish("checked TLS destination", within_watchdog)
            .await;
    }
}
