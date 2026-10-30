//! Outbound 6.T3: the pure decisions a redirect is admitted by and a send is
//! counted by.
//!
//! Every helper here is the production function itself, reached through the
//! doc-hidden `__private` surface. Each reads its arguments and owns nothing:
//!
//! - `client_redirect_destination(policy, initial, current, location)`
//!   resolves one `Location` value against the current URL. It returns the URL
//!   the next send goes to, with no fragment, or the refusal. Same-origin
//!   policy compares against the initial URL's origin. Cross-origin policy
//!   also admits its allowlist, or any HTTPS origin, and refuses an
//!   HTTPS-to-HTTP hop.
//! - `client_next_redirect(policy, followed)` returns the ordinal of the
//!   redirect a follow would spend, or `None` when the allowance is spent.
//! - `client_send_total(retries_sent, redirects_followed)` returns how many
//!   sends an operation has made, or `None` when a `usize` cannot hold the
//!   count.
//!
//! These rows prove the predicates. The component rows of 6.T1 and 6.T2 prove
//! that dispatch runs them.

use camber::__private::{client_next_redirect, client_redirect_destination, client_send_total};
use camber::http::ClientRedirect;
use camber::http::RedirectPolicy;
use reqwest::Url;
use std::num::NonZeroUsize;

/// One way to write an origin, with the identity of the origin it names.
#[derive(Clone, Copy, Debug)]
struct Origin {
    scheme: &'static str,
    /// The index of the host this spelling names. Two spellings with one index
    /// are one host.
    host: usize,
    spelling: &'static str,
    /// The port as written, or `None` to leave the scheme's default.
    port: Option<u16>,
}

impl Origin {
    /// The port a connection to this origin uses.
    fn effective_port(self) -> u16 {
        match (self.port, self.scheme) {
            (Some(port), _) => port,
            (None, "https") => 443,
            (None, _) => 80,
        }
    }

    /// Whether `other` names the same scheme, host, and effective port.
    fn same_as(self, other: Self) -> bool {
        self.scheme == other.scheme
            && self.host == other.host
            && self.effective_port() == other.effective_port()
    }

    /// An absolute URL at this origin.
    fn url(self, path_and_more: &str) -> String {
        let port = self
            .port
            .map_or_else(String::new, |port| format!(":{port}"));
        format!("{}://{}{port}{path_and_more}", self.scheme, self.spelling)
    }
}

const SCHEMES: [&str; 2] = ["http", "https"];

/// Every host spelling, with the index of the host it names. A registered
/// name is compared without regard to case, and an IPv6 address by its value.
const HOSTS: [(usize, &str); 7] = [
    (0, "example.test"),
    (0, "EXAMPLE.test"),
    (0, "Example.Test"),
    (1, "other.test"),
    (2, "127.0.0.1"),
    (3, "[::1]"),
    (3, "[0:0:0:0:0:0:0:1]"),
];

/// Every written port: none, each scheme's default, and two others.
const PORTS: [Option<u16>; 5] = [None, Some(80), Some(443), Some(8080), Some(8443)];

/// Every origin spelling the matrix compares.
fn origins() -> Vec<Origin> {
    let mut origins = Vec::new();
    for scheme in SCHEMES {
        for (host, spelling) in HOSTS {
            origins.extend(PORTS.map(|port| Origin {
                scheme,
                host,
                spelling,
                port,
            }));
        }
    }
    origins
}

fn parsed(url: &str) -> Url {
    Url::parse(url).unwrap_or_else(|error| panic!("the row's own URL {url:?} is invalid: {error}"))
}

/// Resolve `location` for a call that began at `initial` and is now at
/// `current`.
fn destination(initial: &str, current: &str, location: &str) -> Result<String, ClientRedirect> {
    decided(&same_origin(1), initial, current, location)
}

/// Resolve `location` under `policy` for a call that began at `initial` and
/// is now at `current`.
fn decided(
    policy: &RedirectPolicy,
    initial: &str,
    current: &str,
    location: &str,
) -> Result<String, ClientRedirect> {
    client_redirect_destination(
        policy,
        &parsed(initial),
        &parsed(current),
        location.as_bytes(),
    )
    .map(String::from)
}

/// One pair of the matrix: a call that began at `initial`, answered with an
/// absolute `Location` at `target`.
///
/// The same origin is admitted with its path and query kept and its fragment
/// removed. Any other origin is refused as changed.
fn assert_origin_pair(initial: Origin, target: Origin) {
    let start = initial.url("/start?step=1");
    let location = target.url("/next?page=2#fragment");
    let context = format!("{start} redirected to {location}");

    let admitted = client_redirect_destination(
        &same_origin(1),
        &parsed(&start),
        &parsed(&start),
        location.as_bytes(),
    );

    match (initial.same_as(target), admitted) {
        (true, Ok(next)) => {
            assert_eq!(next.path(), "/next", "{context}: path");
            assert_eq!(next.query(), Some("page=2"), "{context}: query");
            assert_eq!(next.fragment(), None, "{context}: fragment");
            assert_eq!(
                next.origin(),
                parsed(&start).origin(),
                "{context}: the admitted URL left the initial origin"
            );
        }
        (false, Err(ClientRedirect::OriginChanged)) => {}
        (same, other) => panic!("{context}: same origin is {same}, and the decision was {other:?}"),
    }
}

/// Scheme, normalized host, and effective port decide the origin, and nothing
/// else does.
fn assert_origin_matrix() {
    let origins = origins();
    let mut admitted = 0_usize;
    for initial in &origins {
        for target in &origins {
            assert_origin_pair(*initial, *target);
            admitted += usize::from(initial.same_as(*target));
        }
    }
    // The matrix is not all refusals: each origin has more than one spelling.
    assert!(
        admitted > origins.len(),
        "the matrix admitted only {admitted} of its pairs"
    );
}

/// The URL every fixed row began at.
const INITIAL: &str = "https://example.test/start";

/// A change of scheme is a change of origin in both directions, whatever port
/// the `Location` writes.
fn assert_scheme_changes_are_refused() {
    let rows = [
        (INITIAL, "http://example.test/next", "HTTPS to HTTP"),
        (
            INITIAL,
            "http://example.test:443/next",
            "HTTPS to HTTP on the HTTPS port",
        ),
        (
            "http://example.test/start",
            "https://example.test/next",
            "HTTP to HTTPS",
        ),
        (
            "http://example.test/start",
            "https://example.test:80/next",
            "HTTP to HTTPS on the HTTP port",
        ),
    ];
    for (initial, location, label) in rows {
        assert_eq!(
            destination(initial, initial, location),
            Err(ClientRedirect::OriginChanged),
            "{label}"
        );
    }
}

/// The URL a call is at when the relative rows resolve.
const CURRENT: &str = "https://example.test/deep/dir/page?old=1";

/// A relative `Location` resolves against the URL the call is at, not the one
/// it began at, and loses its fragment.
fn assert_relative_locations_resolve_against_the_current_url() {
    let rows = [
        (
            "next?x=1#fragment",
            "https://example.test/deep/dir/next?x=1",
        ),
        ("/root", "https://example.test/root"),
        ("../up", "https://example.test/deep/up"),
        (
            "?only=query",
            "https://example.test/deep/dir/page?only=query",
        ),
        ("#only-fragment", "https://example.test/deep/dir/page?old=1"),
        (
            "//example.test/scheme-relative#fragment",
            "https://example.test/scheme-relative",
        ),
        (
            "//EXAMPLE.test:443/explicit-default",
            "https://example.test/explicit-default",
        ),
    ];
    for (location, expected) in rows {
        assert_eq!(
            destination(INITIAL, CURRENT, location),
            Ok(expected.to_owned()),
            "Location {location:?} at {CURRENT}"
        );
    }
    assert_eq!(
        destination(INITIAL, CURRENT, "//other.test/next"),
        Err(ClientRedirect::OriginChanged),
        "a scheme-relative Location to another host"
    );
}

/// A `Location` that is not a URL is refused as invalid.
fn assert_malformed_locations_are_invalid() {
    let rows = [
        "https://[::1/next",
        "https://bad host/next",
        "https://example.test:99999/next",
    ];
    for location in rows {
        assert_eq!(
            destination(INITIAL, CURRENT, location),
            Err(ClientRedirect::InvalidLocation),
            "Location {location:?}"
        );
    }
}

/// Userinfo at the initial origin is an invalid `Location`. A scheme that is
/// not HTTP is another origin.
fn assert_unsendable_locations_are_refused() {
    let rows = [
        (
            "https://user:secret@example.test/next",
            ClientRedirect::InvalidLocation,
        ),
        (
            "https://user@example.test/next",
            ClientRedirect::InvalidLocation,
        ),
        (
            "//user:secret@example.test/next",
            ClientRedirect::InvalidLocation,
        ),
        ("ftp://example.test/next", ClientRedirect::OriginChanged),
        ("file:///etc/hosts", ClientRedirect::OriginChanged),
        ("mailto:ops@example.test", ClientRedirect::OriginChanged),
        ("data:text/plain,next", ClientRedirect::OriginChanged),
        ("javascript:void(0)", ClientRedirect::OriginChanged),
    ];
    for (location, refusal) in rows {
        assert_eq!(
            destination(INITIAL, CURRENT, location),
            Err(refusal),
            "Location {location:?}"
        );
    }
}

fn same_origin(max_hops: usize) -> RedirectPolicy {
    RedirectPolicy::SameOrigin {
        max_hops: NonZeroUsize::new(max_hops).unwrap(),
    }
}

/// `None` has no allowance, whatever was followed.
fn assert_none_admits_no_redirect() {
    for followed in [0, 1, usize::MAX] {
        assert_eq!(
            client_next_redirect(&RedirectPolicy::None, followed),
            None,
            "None after {followed} redirects"
        );
    }
}

/// Exactly `max_hops` redirects are admitted: the last one at the boundary,
/// and none past it.
fn assert_hop_allowance_boundary(max_hops: usize) {
    for policy in [
        same_origin(max_hops),
        RedirectPolicy::cross_origin_https(max_hops).unwrap(),
    ] {
        assert_policy_hop_boundary(&policy, max_hops);
    }
}

/// `policy` admits exactly `max_hops` redirects.
fn assert_policy_hop_boundary(policy: &RedirectPolicy, max_hops: usize) {
    assert_eq!(
        client_next_redirect(policy, 0),
        Some(1),
        "the first of {max_hops} hops"
    );
    assert_eq!(
        client_next_redirect(policy, max_hops - 1),
        Some(max_hops),
        "the last of {max_hops} hops"
    );
    assert_eq!(
        client_next_redirect(policy, max_hops),
        None,
        "a hop past {max_hops}"
    );
    assert_eq!(
        client_next_redirect(policy, usize::MAX),
        None,
        "a full counter under {max_hops} hops"
    );
}

/// The allowlist every cross-origin row reads.
const CDN: &str = "https://cdn.example";

/// A cross-origin policy that permits `origins`.
fn cross_origin(origins: &[&str]) -> RedirectPolicy {
    RedirectPolicy::cross_origin(2, origins).unwrap()
}

/// One cross-origin row: the allowlist, the URL the call is at, the
/// `Location`, and the decision.
type CrossRow = (
    &'static [&'static str],
    &'static str,
    &'static str,
    Result<&'static str, ClientRedirect>,
);

/// The URL every cross-origin call began at.
const START: &str = "https://api.example/start";

/// Every cross-origin decision row.
const CROSS_ROWS: [CrossRow; 27] = [
    (
        &[CDN],
        START,
        "https://cdn.example/file",
        Ok("https://cdn.example/file"),
    ),
    (
        &[CDN],
        START,
        "https://cdn.example:443/f#x",
        Ok("https://cdn.example/f"),
    ),
    (
        &[CDN],
        START,
        "https://cdn.example:8443/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &["https://cdn.example:8443"],
        START,
        "https://cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &["https://cdn.example:8443"],
        START,
        "https://cdn.example:8443/f",
        Ok("https://cdn.example:8443/f"),
    ),
    (
        &[CDN],
        START,
        "https://cdn.example.evil/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "https://evil-cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "https://sub.cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "https://CDN.Example/f",
        Ok("https://cdn.example/f"),
    ),
    (
        &["https://bücher.example"],
        START,
        "https://xn--bcher-kva.example/f",
        Ok("https://xn--bcher-kva.example/f"),
    ),
    (
        &["https://xn--bcher-kva.example"],
        START,
        "https://BÜCHER.example/f",
        Ok("https://xn--bcher-kva.example/f"),
    ),
    (
        &[CDN],
        START,
        "https://cdn.example./f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &["https://cdn.example."],
        START,
        "https://cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &["https://cdn.example."],
        START,
        "https://cdn.example./f",
        Ok("https://cdn.example./f"),
    ),
    (
        &[CDN],
        START,
        "//cdn.example/f",
        Ok("https://cdn.example/f"),
    ),
    (
        &[CDN],
        "http://api.example/start",
        "//cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "//evil.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "ftp://cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "data:text/plain,cdn.example",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "https://user:pw@cdn.example/f",
        Err(ClientRedirect::InvalidLocation),
    ),
    (
        &[CDN],
        START,
        "https://cdn.example@evil.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &[CDN],
        START,
        "https://evil.example\\@cdn.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        &["http://cdn.example"],
        START,
        "http://cdn.example/f",
        Err(ClientRedirect::Downgrade),
    ),
    (
        &[CDN],
        "https://cdn.example/f",
        "http://api.example/start",
        Err(ClientRedirect::Downgrade),
    ),
    (
        &[CDN],
        "https://cdn.example/f",
        "/again",
        Ok("https://cdn.example/again"),
    ),
    (
        &[CDN],
        "https://cdn.example/f",
        "https://api.example/back",
        Ok("https://api.example/back"),
    ),
    (
        &["http://cdn.example"],
        "http://api.example/start",
        "http://cdn.example/f",
        Ok("http://cdn.example/f"),
    ),
];

/// A cross-origin destination is admitted only when its scheme, normalized
/// host, and effective port equal those of the initial origin or one entry.
fn assert_cross_origin_decisions() {
    for (origins, current, location, expected) in CROSS_ROWS {
        assert_eq!(
            decided(&cross_origin(origins), START, current, location),
            expected.map(str::to_owned),
            "allowlist {origins:?} at {current}: Location {location:?}"
        );
    }
}

/// An HTTP call that reaches HTTPS can never return to HTTP, even to its own
/// initial origin.
fn assert_secure_hop_never_returns_to_http() {
    let start = "http://api.example/start";
    assert_eq!(
        decided(
            &cross_origin(&[CDN]),
            start,
            "https://cdn.example/f",
            "http://api.example/back"
        ),
        Err(ClientRedirect::Downgrade)
    );
}

/// The URL an any-HTTPS call began at over plain HTTP.
const HTTP_START: &str = "http://api.example/start";

/// One any-HTTPS row: the URL the call began at, the URL it is at, the
/// `Location`, and the decision.
type AnyHttpsRow = (
    &'static str,
    &'static str,
    &'static str,
    Result<&'static str, ClientRedirect>,
);

/// Every any-HTTPS decision row.
const ANY_HTTPS_ROWS: [AnyHttpsRow; 15] = [
    (
        START,
        START,
        "https://unlisted.example/f",
        Ok("https://unlisted.example/f"),
    ),
    (
        START,
        START,
        "https://unlisted.example:8443/f#x",
        Ok("https://unlisted.example:8443/f"),
    ),
    (
        START,
        START,
        "//other.example/f",
        Ok("https://other.example/f"),
    ),
    (START, START, "https://[::1]/f", Ok("https://[::1]/f")),
    (
        START,
        "https://cdn.example/f",
        "https://api.example/back",
        Ok("https://api.example/back"),
    ),
    (
        START,
        START,
        "http://unlisted.example/f",
        Err(ClientRedirect::Downgrade),
    ),
    (
        START,
        START,
        "http://api.example/start",
        Err(ClientRedirect::Downgrade),
    ),
    (
        START,
        START,
        "ftp://unlisted.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        START,
        START,
        "data:text/plain,unlisted.example",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        START,
        START,
        "https://user:pw@unlisted.example/f",
        Err(ClientRedirect::InvalidLocation),
    ),
    (
        HTTP_START,
        HTTP_START,
        "https://unlisted.example/f",
        Ok("https://unlisted.example/f"),
    ),
    (
        HTTP_START,
        HTTP_START,
        "/again",
        Ok("http://api.example/again"),
    ),
    (
        HTTP_START,
        HTTP_START,
        "http://unlisted.example/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        HTTP_START,
        HTTP_START,
        "http://api.example:8080/f",
        Err(ClientRedirect::OriginChanged),
    ),
    (
        HTTP_START,
        "https://cdn.example/f",
        "http://api.example/back",
        Err(ClientRedirect::Downgrade),
    ),
];

/// An any-HTTPS destination is admitted when it is the initial origin or
/// any HTTPS origin, and never by a step from HTTPS to HTTP.
fn assert_any_https_decisions() {
    let policy = RedirectPolicy::cross_origin_https(2).unwrap();
    for (initial, current, location, expected) in ANY_HTTPS_ROWS {
        assert_eq!(
            decided(&policy, initial, current, location),
            expected.map(str::to_owned),
            "began at {initial}, at {current}: Location {location:?}"
        );
    }
}

/// The count is the first send plus each retry and each redirect, and it is
/// absent instead of wrapped.
fn assert_send_total_is_checked() {
    let widest_retries = usize::try_from(u32::MAX).unwrap();
    let rows: [(u32, usize, Option<usize>, &str); 9] = [
        (0, 0, Some(1), "the first send alone"),
        (3, 0, Some(4), "retries alone"),
        (0, 2, Some(3), "redirects alone"),
        (3, 2, Some(6), "retries and redirects"),
        (
            u32::MAX,
            0,
            widest_retries.checked_add(1),
            "every retry a count can configure",
        ),
        (0, usize::MAX - 1, Some(usize::MAX), "the widest count"),
        (0, usize::MAX, None, "one send past the widest count"),
        (1, usize::MAX - 1, None, "one retry past the widest count"),
        (u32::MAX, usize::MAX, None, "both counters full"),
    ];
    for (retries, redirects, expected, label) in rows {
        assert_eq!(
            client_send_total(retries, redirects),
            expected,
            "{label}: {retries} retries and {redirects} redirects"
        );
    }
}

/// 6.T3
#[test]
fn origin_and_send_accounting_share_production_predicates() {
    assert_origin_matrix();
    assert_scheme_changes_are_refused();
    assert_relative_locations_resolve_against_the_current_url();
    assert_malformed_locations_are_invalid();
    assert_unsendable_locations_are_refused();
    assert_cross_origin_decisions();
    assert_secure_hop_never_returns_to_http();
    assert_any_https_decisions();

    assert_none_admits_no_redirect();
    for max_hops in [1, 2, 7, usize::MAX] {
        assert_hop_allowance_boundary(max_hops);
    }
    assert_send_total_is_checked();
}
