//! A root added with `Client::add_root_certificate` joins the roots every
//! transport of that client trusts, and disables no check.
//!
//! Every row dials a real loopback TLS peer that reports how each handshake
//! ended, so a refusal is proven by a peer that saw the client refuse, and not
//! only by the client's error. Each network row runs buffered and streamed,
//! over the reusable transport and over a transport pinned by an egress
//! policy.
//!
//! | Clause | Row | Mutation the row fails under |
//! |---|---|---|
//! | A PEM is read when it is added | `a_root_is_read_when_it_is_added` | defer parsing to the transport build, or skip a section that is not a certificate |
//! | A self-signed leaf is a root | `an_added_self_signed_leaf_is_trusted` | require `CA:TRUE` of an added root |
//! | Roots accumulate | `added_roots_accumulate` | replace the earlier roots, or read one certificate of a bundle |
//! | No check is disabled | `an_added_root_disables_no_check` | skip the chain, validity, or name check |
//! | Every hop is covered | `added_roots_reach_every_redirect_hop` | trust the added roots on the first hop only |

use crate::client_egress::closing_answer;
use crate::halt::HaltableListener;
use crate::retry_upstream::{answer, client_answer, within_watchdog};
use crate::tls_support::{Validity, generate_leaf};
use crate::untrusted_tls::{NamedTlsEvent, TlsPeerEvent, named_tls_peer};

use camber::__private::{ClientDnsEdge, client_dns_edge};
use camber::RuntimeError;
use camber::http::{self, Client, ClientError, EgressPolicy, RedirectPolicy};
use std::net::{IpAddr, Ipv4Addr};

/// The address every peer listens on, and the name its leaf may hold.
const LOOPBACK: &str = "127.0.0.1";
const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The body a trusted peer answers with.
const BODY: &[u8] = b"trusted";

/// How a row's client reaches its peers.
#[derive(Clone, Copy, Debug)]
enum Transport {
    /// The client's reusable transport, under no egress policy.
    Reusable,
    /// A transport pinned to each admitted origin's checked address.
    Pinned,
}

/// Whether a row streams its response, and the transport it sends over.
type Variant = (bool, Transport);

/// Every way a row sends: buffered and streamed, over each transport.
const VARIANTS: [Variant; 4] = [
    (false, Transport::Reusable),
    (false, Transport::Pinned),
    (true, Transport::Reusable),
    (true, Transport::Pinned),
];

/// A leaf certificate and its key, as PEM.
type Leaf = (Vec<u8>, Vec<u8>);

/// The status and body of one call, or its failure.
type Answer = Result<(u16, Vec<u8>), RuntimeError>;

/// A client over `transport` that trusts each of `roots`.
///
/// A pinned client admits each of `origins` at the loopback address, and no
/// other destination.
fn trusting(transport: Transport, origins: &[&str], roots: &[&[u8]]) -> Client {
    let client = match transport {
        Transport::Reusable => http::client(),
        Transport::Pinned => http::client().egress_policy(
            origins
                .iter()
                .fold(EgressPolicy::public_internet(), |policy, origin| {
                    policy.allow_destination(origin, V4).unwrap()
                }),
        ),
    };
    roots
        .iter()
        .try_fold(client, |client, pem| client.add_root_certificate(pem))
        .unwrap_or_else(|error| panic!("{transport:?}: a root was refused: {error:?}"))
}

/// The HTTPS origin of `peer` at the loopback address.
fn origin(peer: &HaltableListener<NamedTlsEvent>) -> String {
    format!("https://{LOOPBACK}:{}", peer.addr().port())
}

/// The status and body of one GET to `url`, buffered or streamed.
async fn fetch(client: &Client, url: &str, streaming: bool) -> Answer {
    let request = client.request("GET", url)?;
    within_watchdog(client_answer(request, streaming))
        .await
        .unwrap_or_else(|| panic!("{url}: the call never settled"))
}

/// How far the next connection to `peer` got.
async fn reached(peer: &mut HaltableListener<NamedTlsEvent>, row: &str) -> TlsPeerEvent {
    match within_watchdog(peer.recv()).await {
        Some(Some(event)) => event.reached,
        other => panic!("{row}: the peer reported {other:?}"),
    }
}

/// Send one GET to a fresh peer that presents `leaf`, from a client that
/// trusts each of `roots`, and return the answer and what the peer saw.
async fn exchange(
    row: &str,
    leaf: &Leaf,
    roots: &[&[u8]],
    (streaming, transport): Variant,
) -> (Answer, TlsPeerEvent) {
    let mut peer = named_tls_peer(&leaf.0, &leaf.1, closing_answer(BODY)).await;
    let address = origin(&peer);
    let client = trusting(transport, &[&address], roots);
    let result = fetch(&client, &format!("{address}/"), streaming).await;
    let seen = reached(&mut peer, row).await;
    peer.finish(row, within_watchdog).await;
    (result, seen)
}

/// What a row requires of an answer and of what the peer saw.
type Expectation = fn(&str, Answer, &TlsPeerEvent);

/// Require a trusted answer, and a handshake the peer completed.
fn assert_trusted(row: &str, result: Answer, seen: &TlsPeerEvent) {
    assert!(
        matches!(seen, TlsPeerEvent::Completed(_)),
        "{row}: the peer saw {seen:?}"
    );
    assert_eq!(
        result.unwrap_or_else(|error| panic!("{row}: the call failed: {error:?}")),
        (200, BODY.to_vec()),
        "{row}: the answer"
    );
}

/// Require a TLS refusal, and a handshake the client refused.
fn assert_refused(row: &str, result: Answer, seen: &TlsPeerEvent) {
    assert_eq!(
        *seen,
        TlsPeerEvent::HandshakeRefused,
        "{row}: the client must refuse the certificate"
    );
    assert!(
        matches!(result, Err(RuntimeError::Client(ClientError::Tls))),
        "{row}: returned {result:?}"
    );
}

/// A root is parsed when it is added, and only a PEM of certificates is a
/// root. Every refusal is `Configuration`.
#[test]
fn a_root_is_read_when_it_is_added() {
    let (first, first_key) = generate_leaf(&[LOOPBACK], Validity::Current);
    let (second, _) = generate_leaf(&[LOOPBACK], Validity::Current);
    let bundle = [first.as_slice(), second.as_slice()].concat();
    let with_key = [first.as_slice(), first_key.as_slice()].concat();
    let not_a_certificate: &[u8] = b"-----BEGIN CERTIFICATE-----\n\
        bm90IGEgY2VydGlmaWNhdGU=\n\
        -----END CERTIFICATE-----\n";
    let refused: [(&str, &[u8]); 5] = [
        ("an empty PEM", b""),
        ("text that is not PEM", b"not a certificate"),
        ("a private key alone", &first_key),
        ("a certificate beside its key", &with_key),
        (
            "a certificate section that holds no certificate",
            not_a_certificate,
        ),
    ];
    for (row, pem) in refused {
        let refusal = http::client().add_root_certificate(pem).map(drop);
        assert!(
            matches!(
                refusal,
                Err(RuntimeError::Client(ClientError::Configuration))
            ),
            "{row}: returned {refusal:?}"
        );
    }

    let added = http::client()
        .add_root_certificate(&first)
        .and_then(|client| client.add_root_certificate(&bundle))
        .unwrap();
    assert!(format!("{added:?}").contains("added_roots: 3"), "{added:?}");
}

/// The usual local fixture is a self-signed leaf, `CA:FALSE`, for both
/// `localhost` and `127.0.0.1`. Added as a root, it is trusted for either
/// name it holds.
#[tokio::test]
async fn an_added_self_signed_leaf_is_trusted() {
    let leaf = generate_leaf(&["localhost", LOOPBACK], Validity::Current);
    for variant in VARIANTS {
        let row = format!("the address, {variant:?}");
        let (result, seen) = exchange(&row, &leaf, &[&leaf.0], variant).await;
        assert_trusted(&row, result, &seen);
    }
    for streaming in [false, true] {
        let row = format!("the name, streaming {streaming}");
        let (result, seen) = exchange_by_name(&row, &leaf, streaming).await;
        assert_trusted(&row, result, &seen);
    }
}

/// Send one GET to `localhost`, which the lookup edge answers with the
/// loopback address, from a pinned client that trusts `leaf`.
async fn exchange_by_name(row: &str, leaf: &Leaf, streaming: bool) -> (Answer, TlsPeerEvent) {
    let mut peer = named_tls_peer(&leaf.0, &leaf.1, closing_answer(BODY)).await;
    let name = format!("https://localhost:{}", peer.addr().port());
    let edge = ClientDnsEdge::new();
    edge.answer(&[V4]);
    let policy = EgressPolicy::public_internet()
        .allow_destination(&name, V4)
        .unwrap();
    let client = client_dns_edge(http::client().egress_policy(policy), &edge)
        .add_root_certificate(&leaf.0)
        .unwrap();
    let result = fetch(&client, &format!("{name}/"), streaming).await;
    let seen = reached(&mut peer, row).await;
    peer.finish(row, within_watchdog).await;
    (result, seen)
}

/// A second call adds to the first, and every certificate of a bundle is
/// added.
#[tokio::test]
async fn added_roots_accumulate() {
    let leaves: [_; 3] = std::array::from_fn(|_| generate_leaf(&[LOOPBACK], Validity::Current));
    let bundle = [leaves[1].0.as_slice(), leaves[2].0.as_slice()].concat();
    let roots: &[&[u8]] = &[&leaves[0].0, &bundle];
    for variant in VARIANTS {
        for (index, leaf) in leaves.iter().enumerate() {
            let row = format!("leaf {index}, {variant:?}");
            let (result, seen) = exchange(&row, leaf, roots, variant).await;
            assert_trusted(&row, result, &seen);
        }
    }
}

/// What a refused row's peer presents, and what its client trusts.
struct Unverified {
    row: &'static str,
    /// The one name the presented leaf holds.
    presented: &'static str,
    validity: Validity,
    trusted: Trusted,
}

/// Which root a refused row's client adds.
#[derive(Clone, Copy)]
enum Trusted {
    /// None: the client trusts only the platform's roots.
    Nothing,
    /// Another leaf for the same address, which did not sign the peer's.
    Stranger,
    /// The leaf the peer presents.
    Presented,
}

const UNVERIFIED: [Unverified; 5] = [
    Unverified {
        row: "no root added",
        presented: LOOPBACK,
        validity: Validity::Current,
        trusted: Trusted::Nothing,
    },
    Unverified {
        row: "a root that did not sign the leaf",
        presented: LOOPBACK,
        validity: Validity::Current,
        trusted: Trusted::Stranger,
    },
    Unverified {
        row: "a leaf for another name",
        presented: "other.trust.test",
        validity: Validity::Current,
        trusted: Trusted::Presented,
    },
    Unverified {
        row: "an expired leaf",
        presented: LOOPBACK,
        validity: Validity::Expired,
        trusted: Trusted::Presented,
    },
    Unverified {
        row: "a leaf that is not yet valid",
        presented: LOOPBACK,
        validity: Validity::NotYetValid,
        trusted: Trusted::Presented,
    },
];

/// With a root added, the chain, the validity window, and the name are each
/// still checked. A client that added no root still refuses a local leaf.
#[tokio::test]
async fn an_added_root_disables_no_check() {
    for case in &UNVERIFIED {
        let leaf = generate_leaf(&[case.presented], case.validity);
        let stranger = generate_leaf(&[LOOPBACK], Validity::Current).0;
        let roots: &[&[u8]] = match case.trusted {
            Trusted::Nothing => &[],
            Trusted::Stranger => &[&stranger],
            Trusted::Presented => &[&leaf.0],
        };
        for variant in VARIANTS {
            let row = format!("{}, {variant:?}", case.row);
            let (result, seen) = exchange(&row, &leaf, roots, variant).await;
            assert_refused(&row, result, &seen);
        }
    }
}

/// An any-HTTPS redirect from one peer to another is verified against the
/// added roots at each hop: followed when both roots are added, and refused
/// at the second hop when only the first is.
#[tokio::test]
async fn added_roots_reach_every_redirect_hop() {
    let leaves = [
        generate_leaf(&[LOOPBACK], Validity::Current),
        generate_leaf(&[LOOPBACK], Validity::Current),
    ];
    let both: &[&[u8]] = &[&leaves[0].0, &leaves[1].0];
    let first_only: &[&[u8]] = &[&leaves[0].0];
    let cases: [(&str, &[&[u8]], Expectation); 2] = [
        ("both roots", both, assert_trusted),
        ("the first root only", first_only, assert_refused),
    ];
    for (case, roots, expect) in cases {
        for variant in VARIANTS {
            let row = format!("{case}, {variant:?}");
            let (result, seen) = redirected_exchange(&row, &leaves, roots, variant).await;
            expect(&row, result, &seen);
        }
    }
}

/// Send one GET to a fresh peer that presents the first leaf and redirects
/// to a fresh peer that presents the second, under an any-HTTPS redirect
/// policy. Return the answer and what the second peer saw.
///
/// The first handshake must complete: only the hop is in question.
async fn redirected_exchange(
    row: &str,
    [start_leaf, landing_leaf]: &[Leaf; 2],
    roots: &[&[u8]],
    (streaming, transport): Variant,
) -> (Answer, TlsPeerEvent) {
    let mut second = named_tls_peer(&landing_leaf.0, &landing_leaf.1, closing_answer(BODY)).await;
    let landing = format!("{}/landed", origin(&second));
    let moved = answer(
        "302 Found",
        &[("Location", &landing), ("Connection", "close")],
        b"",
    );
    let mut first = named_tls_peer(&start_leaf.0, &start_leaf.1, moved).await;
    let start = origin(&first);
    let client = trusting(transport, &[&start, &origin(&second)], roots)
        .redirect_policy(RedirectPolicy::cross_origin_https(2).unwrap());
    let result = fetch(&client, &format!("{start}/start"), streaming).await;
    let first_seen = reached(&mut first, row).await;
    let second_seen = reached(&mut second, row).await;
    first.finish(row, within_watchdog).await;
    second.finish(row, within_watchdog).await;
    assert!(
        matches!(first_seen, TlsPeerEvent::Completed(_)),
        "{row}: the first peer saw {first_seen:?}"
    );
    (result, second_seen)
}
