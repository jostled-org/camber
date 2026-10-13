//! 1.T1: the destination-policy values, and the one decision that reads them.
//!
//! Every policy here is built through the public constructors. Every verdict
//! is the production decision itself, reached through the doc-hidden
//! `__private` surface as `client_egress_decision(&EgressPolicy, &str, IpAddr)
//! -> Result<(), RuntimeError>`. It reads a policy, the target URL of a
//! request, and one address that target would be dialed at. A literal target
//! is given its own address. This proves the classification only. No socket is
//! opened and no name is resolved: dial confinement belongs to the routing
//! root.
//!
//! Clause → row → the mutation that turns the row red:
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | No policy keeps private access | `assert_unrestricted_permits_private_destinations` | classify under the unrestricted policy |
//! | Each IPv4 interval is refused | `assert_intervals_are_exact` over `V4_REFUSED`, `assert_ipv4_middles_are_refused` | remove one interval, or move one bound |
//! | Each IPv6 interval is refused | `assert_intervals_are_exact` over `V6_REFUSED` | remove one interval, or move one bound |
//! | Mapped IPv6 follows the IPv4 rule | `assert_class`, the mapped half | classify a mapped address as IPv6 |
//! | Numeric IPv4 is canonical first | `assert_numeric_ipv4_is_canonical` | classify the spelling, not the address |
//! | A zone is refused | `assert_zones_are_refused` | strip the zone and continue |
//! | Port, case, and root dot normalize | `assert_origin_spellings_are_one_origin`, `assert_generated_spellings` | normalize only one comparison side |
//! | An origin has no other URL part | `assert_origin_inputs_are_refused` | ignore a user name, a password, a query, a fragment, or a path |
//! | An exception is exact | `assert_exceptions_are_exact` | match a host suffix, or drop the port, scheme, or address |
//! | A repeated exception changes nothing | `assert_duplicates_are_idempotent` | refuse or widen on a repeat |
//! | HTTPS-only outranks an exception | `assert_https_only_is_not_weakened` | read the exception before the scheme |
//! | No exception without a restriction | `assert_unrestricted_takes_no_exception` | accept an unrestricted exception |
//! | Some addresses are never excepted | `assert_unexceptable_addresses` | accept unspecified, multicast, or broadcast |
//! | A literal origin names its address | `assert_literal_origins_match_their_address` | accept a literal with another address |
//! | A literal target is dialed at itself | `assert_literal_targets_are_dialed_at_themselves` | classify the address without the literal |

use crate::deterministic::{DeterministicCase, DeterministicGenerator};

use camber::__private::client_egress_decision;
use camber::RuntimeError;
use camber::http::ClientEgress;
use camber::http::ClientError;
use camber::http::ClientInput;
use camber::http::EgressPolicy;
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// What the production decision said about one target at one address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Permitted,
    Scheme,
    Address,
}

/// The production decision, with its result type written out.
pub(crate) fn decide(
    policy: &EgressPolicy,
    url: &str,
    address: IpAddr,
) -> Result<(), RuntimeError> {
    client_egress_decision(policy, url, address)
}

/// The verdict of the production decision.
///
/// The decision resolves nothing, so a resolution-limit refusal, or any
/// failure that is not an egress refusal, is a defect of the row.
fn verdict(policy: &EgressPolicy, url: &str, address: IpAddr) -> Verdict {
    match decide(policy, url, address) {
        Ok(()) => Verdict::Permitted,
        Err(RuntimeError::Client(ClientError::Egress(ClientEgress::Scheme))) => Verdict::Scheme,
        Err(RuntimeError::Client(ClientError::Egress(ClientEgress::Address))) => Verdict::Address,
        Err(other) => panic!("{url} at {address}: the decision returned {other:?}"),
    }
}

/// The restricted policy with no exception.
fn restricted() -> EgressPolicy {
    EgressPolicy::public_internet()
}

/// `policy` with one more exception the row requires to be accepted.
pub(crate) fn excepting(policy: EgressPolicy, origin: &str, address: IpAddr) -> EgressPolicy {
    let added: Result<EgressPolicy, RuntimeError> = policy.allow_destination(origin, address);
    added.unwrap_or_else(|error| {
        panic!("{origin} at {address}: the exception was refused: {error:?}")
    })
}

/// Require `policy` to refuse the exception as invalid policy configuration.
fn assert_exception_refused(policy: EgressPolicy, origin: &str, address: IpAddr, label: &str) {
    let added: Result<EgressPolicy, RuntimeError> = policy.allow_destination(origin, address);
    match added {
        Err(RuntimeError::Client(ClientError::InvalidRequest(ClientInput::EgressPolicy))) => (),
        Err(other) => panic!("{label}: {origin:?} at {address} was refused as {other:?}"),
        Ok(_) => panic!("{label}: {origin:?} at {address} was accepted"),
    }
}

/// Require `target` at `address` to be refused under the restricted policy,
/// and permitted once `origin` is excepted at that address.
fn assert_exception_admits(origin: &str, target: &str, address: IpAddr, label: &str) {
    assert_eq!(
        verdict(&restricted(), target, address),
        Verdict::Address,
        "{label}: refused before the exception"
    );
    let policy = excepting(restricted(), origin, address);
    assert_eq!(
        verdict(&policy, target, address),
        Verdict::Permitted,
        "{label}"
    );
}

/// The root URL whose host is `address` itself.
fn literal_url(scheme: &str, address: IpAddr) -> String {
    match address {
        IpAddr::V4(literal) => format!("{scheme}://{literal}/"),
        IpAddr::V6(literal) => format!("{scheme}://[{literal}]/"),
    }
}

/// A public name every resolved-address row is asked under.
const PUBLIC_NAME_URL: &str = "http://example.com/";

/// Require one address to be classified as `expected`, as the literal host of
/// a target and as the address a name resolved to.
fn assert_one_form(policy: &EgressPolicy, address: IpAddr, expected: Verdict, label: &str) {
    let literal = literal_url("http", address);
    assert_eq!(
        verdict(policy, &literal, address),
        expected,
        "{label}: the literal target {literal}"
    );
    assert_eq!(
        verdict(policy, PUBLIC_NAME_URL, address),
        expected,
        "{label}: a name that resolved to {address}"
    );
}

/// Require one address to be classified as `expected`. An IPv4 address is
/// asked again in its IPv4-mapped IPv6 form, which follows the same rule.
fn assert_class(policy: &EgressPolicy, address: IpAddr, expected: Verdict, label: &str) {
    assert_one_form(policy, address, expected, label);
    if let IpAddr::V4(plain) = address {
        let mapped = IpAddr::V6(plain.to_ipv6_mapped());
        assert_one_form(policy, mapped, expected, label);
    }
}

/// One refused interval of addresses of one family, with the verdict of the
/// address on each side. `None` names a side the address space ends at.
struct Interval {
    label: &'static str,
    first: IpAddr,
    last: IpAddr,
    below: Option<Verdict>,
    above: Option<Verdict>,
}

const fn v4(
    label: &'static str,
    first: [u8; 4],
    last: [u8; 4],
    below: Option<Verdict>,
    above: Option<Verdict>,
) -> Interval {
    Interval {
        label,
        first: IpAddr::V4(Ipv4Addr::new(first[0], first[1], first[2], first[3])),
        last: IpAddr::V4(Ipv4Addr::new(last[0], last[1], last[2], last[3])),
        below,
        above,
    }
}

const OPEN: Option<Verdict> = Some(Verdict::Permitted);
const SHUT: Option<Verdict> = Some(Verdict::Address);

/// Every IPv4 interval the restricted policy refuses. `224/4` and `240/4`
/// touch, so each has the other on one side.
const V4_REFUSED: [Interval; 15] = [
    v4("0/8", [0, 0, 0, 0], [0, 255, 255, 255], None, OPEN),
    v4("10/8", [10, 0, 0, 0], [10, 255, 255, 255], OPEN, OPEN),
    v4(
        "100.64/10",
        [100, 64, 0, 0],
        [100, 127, 255, 255],
        OPEN,
        OPEN,
    ),
    v4("127/8", [127, 0, 0, 0], [127, 255, 255, 255], OPEN, OPEN),
    v4(
        "169.254/16",
        [169, 254, 0, 0],
        [169, 254, 255, 255],
        OPEN,
        OPEN,
    ),
    v4(
        "172.16/12",
        [172, 16, 0, 0],
        [172, 31, 255, 255],
        OPEN,
        OPEN,
    ),
    v4("192.0.0/24", [192, 0, 0, 0], [192, 0, 0, 255], OPEN, OPEN),
    v4("192.0.2/24", [192, 0, 2, 0], [192, 0, 2, 255], OPEN, OPEN),
    v4(
        "192.88.99/24",
        [192, 88, 99, 0],
        [192, 88, 99, 255],
        OPEN,
        OPEN,
    ),
    v4(
        "192.168/16",
        [192, 168, 0, 0],
        [192, 168, 255, 255],
        OPEN,
        OPEN,
    ),
    v4(
        "198.18/15",
        [198, 18, 0, 0],
        [198, 19, 255, 255],
        OPEN,
        OPEN,
    ),
    v4(
        "198.51.100/24",
        [198, 51, 100, 0],
        [198, 51, 100, 255],
        OPEN,
        OPEN,
    ),
    v4(
        "203.0.113/24",
        [203, 0, 113, 0],
        [203, 0, 113, 255],
        OPEN,
        OPEN,
    ),
    v4("224/4", [224, 0, 0, 0], [239, 255, 255, 255], OPEN, SHUT),
    v4("240/4", [240, 0, 0, 0], [255, 255, 255, 255], SHUT, None),
];

/// Require the address beside one bound to exist exactly when the row states
/// a verdict for it, and to be given that verdict.
fn assert_edge(
    policy: &EgressPolicy,
    beside: Option<IpAddr>,
    expected: Option<Verdict>,
    label: &str,
) {
    match (beside, expected) {
        (Some(address), Some(expected)) => assert_class(policy, address, expected, label),
        (None, None) => (),
        (beside, expected) => panic!("{label}: the row states {expected:?} for {beside:?}"),
    }
}

/// The side of an address its neighbour is on.
#[derive(Clone, Copy)]
enum Side {
    Below,
    Above,
}

/// The address one step from `address` in its own family, or `None` where the
/// address space ends.
fn beside(address: IpAddr, side: Side) -> Option<IpAddr> {
    match (address, side) {
        (IpAddr::V4(plain), Side::Below) => u32::from(plain)
            .checked_sub(1)
            .map(|bits| IpAddr::V4(Ipv4Addr::from(bits))),
        (IpAddr::V4(plain), Side::Above) => u32::from(plain)
            .checked_add(1)
            .map(|bits| IpAddr::V4(Ipv4Addr::from(bits))),
        (IpAddr::V6(plain), Side::Below) => u128::from(plain)
            .checked_sub(1)
            .map(|bits| IpAddr::V6(Ipv6Addr::from(bits))),
        (IpAddr::V6(plain), Side::Above) => u128::from(plain)
            .checked_add(1)
            .map(|bits| IpAddr::V6(Ipv6Addr::from(bits))),
    }
}

/// Each interval is refused at its first and last address, and the address on
/// each side keeps its own rule.
fn assert_intervals_are_exact(policy: &EgressPolicy, intervals: &[Interval]) {
    for interval in intervals {
        let label = interval.label;
        assert!(
            interval.first < interval.last,
            "{label}: the interval is empty"
        );
        for inside in [interval.first, interval.last] {
            assert_class(policy, inside, Verdict::Address, label);
        }
        let below = beside(interval.first, Side::Below);
        let above = beside(interval.last, Side::Above);
        assert_edge(policy, below, interval.below, label);
        assert_edge(policy, above, interval.above, label);
    }
}

/// The address halfway through one IPv4 interval.
fn ipv4_middle(interval: &Interval) -> IpAddr {
    match (interval.first, interval.last) {
        (IpAddr::V4(first), IpAddr::V4(last)) => {
            let (first, last) = (u32::from(first), u32::from(last));
            IpAddr::V4(Ipv4Addr::from(first + (last - first) / 2))
        }
        (first, last) => panic!(
            "{}: {first} to {last} is not an IPv4 interval",
            interval.label
        ),
    }
}

/// Each IPv4 interval is refused at its middle address too.
///
/// No IPv6 middle is asked: the middle of the lowest interval is not a
/// boundary of anything, and the named addresses below cover its classes.
fn assert_ipv4_middles_are_refused(policy: &EgressPolicy) {
    for interval in &V4_REFUSED {
        assert_class(
            policy,
            ipv4_middle(interval),
            Verdict::Address,
            interval.label,
        );
    }
}

/// The interval from `first` to `last`, each written as its first two groups.
/// Every later group is zero in `first` and all ones in `last`.
const fn v6(
    label: &'static str,
    first: [u16; 2],
    last: [u16; 2],
    below: Option<Verdict>,
    above: Option<Verdict>,
) -> Interval {
    const FULL: u16 = 0xffff;
    Interval {
        label,
        first: IpAddr::V6(Ipv6Addr::new(first[0], first[1], 0, 0, 0, 0, 0, 0)),
        last: IpAddr::V6(Ipv6Addr::new(
            last[0], last[1], FULL, FULL, FULL, FULL, FULL, FULL,
        )),
        below,
        above,
    }
}

/// Every IPv6 interval the restricted policy refuses: both sides of
/// `2000::/3`, and the four intervals inside it.
const V6_REFUSED: [Interval; 6] = [
    v6("below 2000::/3", [0, 0], [0x1fff, 0xffff], None, OPEN),
    v6("2001::/23", [0x2001, 0], [0x2001, 0x01ff], OPEN, OPEN),
    v6(
        "2001:db8::/32",
        [0x2001, 0x0db8],
        [0x2001, 0x0db8],
        OPEN,
        OPEN,
    ),
    v6("2002::/16", [0x2002, 0], [0x2002, 0xffff], OPEN, OPEN),
    v6("3fff::/20", [0x3fff, 0], [0x3fff, 0x0fff], OPEN, OPEN),
    v6("above 2000::/3", [0x4000, 0], [0xffff, 0xffff], OPEN, None),
];

/// Parse one fixture address.
pub(crate) fn ip(text: &str) -> IpAddr {
    text.parse()
        .unwrap_or_else(|error| panic!("{text:?} is not a fixture address: {error}"))
}

/// Addresses an operator would name, each refused by the restricted policy.
const NAMED_REFUSED: [&str; 14] = [
    "127.0.0.1",
    "10.0.0.1",
    "192.168.1.1",
    "169.254.169.254",
    "100.100.100.200",
    "255.255.255.255",
    "::1",
    "fe80::1",
    "fec0::1",
    "fc00::1",
    "fd00:ec2::254",
    "ff02::1",
    "64:ff9b::808:808",
    "100::1",
];

/// Ordinary public unicast addresses, each permitted by the restricted policy.
const NAMED_PUBLIC: [&str; 6] = [
    "1.1.1.1",
    "8.8.8.8",
    "93.184.216.34",
    "2606:4700:4700::1111",
    "2001:4860:4860::8888",
    "2a00:1450:4001::200e",
];

/// The named addresses are classified as an operator expects.
fn assert_named_addresses(policy: &EgressPolicy) {
    for refused in NAMED_REFUSED {
        assert_class(policy, ip(refused), Verdict::Address, refused);
    }
    for public in NAMED_PUBLIC {
        assert_class(policy, ip(public), Verdict::Permitted, public);
    }
}

/// The unrestricted policy permits every address the restricted one refuses.
fn assert_unrestricted_permits_private_destinations() {
    let policy = EgressPolicy::unrestricted();
    for interval in V4_REFUSED.iter().chain(&V6_REFUSED) {
        for inside in [interval.first, interval.last] {
            assert_class(&policy, inside, Verdict::Permitted, interval.label);
        }
    }
    for named in NAMED_REFUSED.into_iter().chain(NAMED_PUBLIC) {
        assert_class(&policy, ip(named), Verdict::Permitted, named);
    }
}

/// One nonstandard IPv4 spelling, the address URL parsing gives it, and the
/// verdict of that address. `010` is octal eight, not ten.
const NUMERIC_TARGETS: [(&str, &str, Verdict); 10] = [
    ("http://2130706433/", "127.0.0.1", Verdict::Address),
    ("http://0x7f000001/", "127.0.0.1", Verdict::Address),
    ("http://0x7f.1/", "127.0.0.1", Verdict::Address),
    ("http://0177.0.0.1/", "127.0.0.1", Verdict::Address),
    ("http://127.1/", "127.0.0.1", Verdict::Address),
    ("http://0xa.0.0.1/", "10.0.0.1", Verdict::Address),
    ("http://012.0.0.1/", "10.0.0.1", Verdict::Address),
    ("http://134744072/", "8.8.8.8", Verdict::Permitted),
    ("http://0x8.0x8.0x8.0x8/", "8.8.8.8", Verdict::Permitted),
    ("http://010.010.010.010/", "8.8.8.8", Verdict::Permitted),
];

/// A numeric spelling is the address it parses to, in the classification and
/// in an exception.
fn assert_numeric_ipv4_is_canonical() {
    let policy = restricted();
    for (target, address, expected) in NUMERIC_TARGETS {
        assert_eq!(verdict(&policy, target, ip(address)), expected, "{target}");
    }

    let loopback = ip("127.0.0.1");
    let excepted = excepting(restricted(), "http://0x7f.1:8080", loopback);
    for target in [
        "http://127.0.0.1:8080/",
        "http://2130706433:8080/",
        "http://0177.0.0.1:8080/",
    ] {
        assert_eq!(
            verdict(&excepted, target, loopback),
            Verdict::Permitted,
            "{target}"
        );
    }
    assert_eq!(
        verdict(&excepted, "http://2130706434:8080/", ip("127.0.0.2")),
        Verdict::Address,
        "the next numeric address is another destination"
    );
}

/// An IPv6 zone identifier is refused in an exception, and a zoned target is
/// no target a request could name, so no unzoned exception admits it.
fn assert_zones_are_refused() {
    let link_local = ip("fe80::1");
    for origin in ["http://[fe80::1%25eth0]:8080", "http://[fe80::1%eth0]:8080"] {
        assert_exception_refused(restricted(), origin, link_local, "zone identifier");
    }
    let policy = excepting(restricted(), "http://[fe80::1]:8080", link_local);
    assert_eq!(
        verdict(&policy, "http://[fe80::1]:8080/", link_local),
        Verdict::Permitted,
        "the unzoned literal is the excepted origin"
    );
    for target in [
        "http://[fe80::1%25eth0]:8080/",
        "http://[fe80::1%eth0]:8080/",
    ] {
        match decide(&policy, target, link_local) {
            Err(RuntimeError::Client(ClientError::InvalidRequest(ClientInput::Url))) => (),
            other => panic!("{target}: a zoned target was decided as {other:?}"),
        }
    }
}

/// The private address every name-origin exception row admits.
pub(crate) const EXCEPTED: &str = "10.20.30.40";

/// One origin in two spellings: the one configured and the one requested.
const SAME_ORIGIN: [(&str, &str); 12] = [
    ("http://service.internal", "http://service.internal:80/"),
    ("http://service.internal:80", "http://service.internal/"),
    ("https://service.internal", "https://service.internal:443/"),
    ("https://service.internal:443/", "https://service.internal/"),
    (
        "http://Service.INTERNAL:8080",
        "http://service.internal:8080/",
    ),
    (
        "http://service.internal:8080",
        "http://SERVICE.Internal:8080/",
    ),
    (
        "http://service.internal.:8080",
        "http://service.internal:8080/",
    ),
    (
        "http://service.internal:8080",
        "http://service.internal.:8080/",
    ),
    ("http://service.internal.", "http://service.internal/"),
    (
        "HTTP://service.internal:8080",
        "http://service.internal:8080/",
    ),
    (
        "http://service.internal:8080/",
        "http://service.internal:8080",
    ),
    (
        "http://service.internal:8080",
        "http://service.internal:8080/deep/path?page=2",
    ),
];

/// A default port, a letter case, and a root dot do not make another origin,
/// whichever side spells them.
fn assert_origin_spellings_are_one_origin() {
    let address = ip(EXCEPTED);
    for (configured, requested) in SAME_ORIGIN {
        let label = format!("{configured} then {requested}");
        assert_exception_admits(configured, requested, address, &label);
    }
}

/// Origins that are not an absolute HTTP(S) origin with at most a root path.
const REFUSED_ORIGINS: [&str; 18] = [
    "http://user@service.internal",
    "http://user:secret@service.internal",
    "http://:secret@service.internal",
    "http://user:@service.internal",
    "http://service.internal/?page=2",
    "http://service.internal?page=2",
    "http://service.internal/#part",
    "http://service.internal#part",
    "http://service.internal/path",
    "http://service.internal//",
    "ftp://service.internal",
    "ws://service.internal",
    "service.internal:8080",
    "service.internal",
    "//service.internal",
    "/service.internal",
    "http://",
    "",
];

/// Spellings of the one origin `http://service.internal`. An empty userinfo
/// delimiter holds no user name and no password, so it is no other origin.
const BARE_ORIGINS: [&str; 4] = [
    "http://service.internal",
    "http://service.internal/",
    "http://@service.internal",
    "http://:@service.internal",
];

/// An origin with a user name, a password, a query, a fragment, a path,
/// another scheme, or no scheme is refused, and every bare spelling is
/// accepted.
fn assert_origin_inputs_are_refused() {
    let address = ip(EXCEPTED);
    for origin in REFUSED_ORIGINS {
        assert_exception_refused(restricted(), origin, address, "origin input");
    }
    for origin in BARE_ORIGINS {
        let policy = excepting(restricted(), origin, address);
        assert_eq!(
            verdict(&policy, "http://service.internal/", address),
            Verdict::Permitted,
            "{origin}"
        );
    }
}

/// The origin every exactness row excepts, at [`EXCEPTED`].
const EXACT_ORIGIN: &str = "http://service.internal:8080";

/// The one target [`EXACT_ORIGIN`] admits.
const EXACT_TARGET: &str = "http://service.internal:8080/";

/// Targets one exception does not admit, each with the address it is asked at.
const NOT_EXCEPTED: [(&str, &str, &str); 10] = [
    ("another scheme", "https://service.internal:8080/", EXCEPTED),
    ("another port", "http://service.internal:8081/", EXCEPTED),
    ("the default port", "http://service.internal/", EXCEPTED),
    ("a subdomain", "http://api.service.internal:8080/", EXCEPTED),
    (
        "a longer first label",
        "http://xservice.internal:8080/",
        EXCEPTED,
    ),
    ("the parent name", "http://internal:8080/", EXCEPTED),
    (
        "a longer name",
        "http://service.internal.example.com:8080/",
        EXCEPTED,
    ),
    ("another address", EXACT_TARGET, "10.20.30.41"),
    ("another private address", EXACT_TARGET, "127.0.0.1"),
    (
        "the address as a literal origin",
        "http://10.20.30.40:8080/",
        EXCEPTED,
    ),
];

/// Require `policy` to admit [`EXACT_TARGET`] and nothing beside it.
fn assert_only_exact_target(policy: &EgressPolicy, label: &str) {
    assert_eq!(
        verdict(policy, EXACT_TARGET, ip(EXCEPTED)),
        Verdict::Permitted,
        "{label}: the excepted target"
    );
    for (row, target, address) in NOT_EXCEPTED {
        assert_eq!(
            verdict(policy, target, ip(address)),
            Verdict::Address,
            "{label}: {row}"
        );
    }
    assert_eq!(
        verdict(policy, EXACT_TARGET, ip("93.184.216.34")),
        Verdict::Permitted,
        "{label}: a public address needs no exception"
    );
}

/// An exception grants its scheme, name, port, and address, and no other.
fn assert_exceptions_are_exact() {
    let policy = excepting(restricted(), EXACT_ORIGIN, ip(EXCEPTED));
    assert_only_exact_target(&policy, "one exception");

    let crossed = excepting(restricted(), "http://service.internal:443", ip(EXCEPTED));
    assert_eq!(
        verdict(&crossed, "https://service.internal/", ip(EXCEPTED)),
        Verdict::Address,
        "an equal port under another scheme is another origin"
    );
}

/// A repeated exception is accepted in every spelling and grants nothing new.
fn assert_duplicates_are_idempotent() {
    let address = ip(EXCEPTED);
    let mapped = ip("::ffff:10.20.30.40");
    let repeats = [
        (EXACT_ORIGIN, address),
        (EXACT_ORIGIN, address),
        ("http://Service.Internal.:8080/", address),
        (EXACT_ORIGIN, mapped),
    ];
    let policy = repeats
        .into_iter()
        .fold(restricted(), |policy, (origin, address)| {
            excepting(policy, origin, address)
        });
    assert_only_exact_target(&policy, "a repeated exception");
}

/// The policy that refuses HTTP, with [`EXACT_ORIGIN`] excepted first.
fn https_only_after_exception() -> EgressPolicy {
    excepting(restricted(), EXACT_ORIGIN, ip(EXCEPTED)).https_only()
}

/// HTTPS-only refuses HTTP before the address is read, and no exception
/// admits an HTTP origin under it.
///
/// An HTTP exception added after the restriction is accepted and kept without
/// effect: the scheme is read before any exception.
fn assert_https_only_is_not_weakened() {
    let address = ip(EXCEPTED);
    let public = ip("93.184.216.34");
    let policy = restricted().https_only();
    assert_eq!(
        verdict(&policy, "http://example.com/", public),
        Verdict::Scheme
    );
    assert_eq!(
        verdict(&policy, "https://example.com/", public),
        Verdict::Permitted
    );
    assert_eq!(
        verdict(&policy, "http://example.com/", address),
        Verdict::Scheme
    );
    assert_eq!(
        verdict(&policy, "https://example.com/", address),
        Verdict::Address
    );

    let before = https_only_after_exception();
    assert_eq!(verdict(&before, EXACT_TARGET, address), Verdict::Scheme);

    let kept = excepting(policy, EXACT_ORIGIN, address);
    assert_eq!(verdict(&kept, EXACT_TARGET, address), Verdict::Scheme);

    let secure = excepting(
        restricted().https_only(),
        "https://service.internal:8443",
        address,
    );
    assert_eq!(
        verdict(&secure, "https://service.internal:8443/", address),
        Verdict::Permitted,
        "an HTTPS exception holds under HTTPS-only"
    );
    assert_eq!(
        verdict(&secure, "http://service.internal:8443/", address),
        Verdict::Scheme,
        "an HTTPS exception admits no HTTP origin"
    );
}

/// An exception on the unrestricted policy is refused, not ignored.
fn assert_unrestricted_takes_no_exception() {
    for (origin, address) in [
        (EXACT_ORIGIN, EXCEPTED),
        ("https://example.com", "93.184.216.34"),
        ("http://127.0.0.1:8080", "127.0.0.1"),
    ] {
        assert_exception_refused(
            EgressPolicy::unrestricted(),
            origin,
            ip(address),
            "unrestricted",
        );
    }
}

/// Unspecified, multicast, and broadcast addresses, plain and mapped.
const UNEXCEPTABLE: [&str; 11] = [
    "0.0.0.0",
    "::",
    "224.0.0.1",
    "239.255.255.255",
    "255.255.255.255",
    "ff00::",
    "ff02::1",
    "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
    "::ffff:0.0.0.0",
    "::ffff:224.0.0.1",
    "::ffff:255.255.255.255",
];

/// Refused unicast addresses an exact exception may admit.
const EXCEPTABLE: [&str; 10] = [
    "127.0.0.1",
    "10.0.0.1",
    "169.254.169.254",
    "192.0.2.1",
    "240.0.0.1",
    "::1",
    "fe80::1",
    "fc00::1",
    "2001:db8::1",
    "2002::1",
];

/// No exception admits an unspecified, multicast, or broadcast address, under
/// a name or as its own literal. Any other refused unicast address is admitted.
fn assert_unexceptable_addresses() {
    for refused in UNEXCEPTABLE {
        let address = ip(refused);
        assert_exception_refused(restricted(), EXACT_ORIGIN, address, "unexceptable");
        let literal = literal_url("http", address);
        assert_exception_refused(restricted(), &literal, address, "unexceptable literal");
    }
    for admitted in EXCEPTABLE {
        let address = ip(admitted);
        assert_exception_admits(EXACT_ORIGIN, EXACT_TARGET, address, admitted);
    }
}

/// A literal origin and an address it is, in a plain or a mapped form.
const LITERAL_MATCHES: [(&str, &str); 5] = [
    ("http://127.0.0.1:8080", "127.0.0.1"),
    ("http://[::1]:8080", "::1"),
    ("http://[::ffff:127.0.0.1]:8080", "127.0.0.1"),
    ("http://127.0.0.1:8080", "::ffff:127.0.0.1"),
    ("http://2130706433:8080", "127.0.0.1"),
];

/// A literal origin and an address it is not.
const LITERAL_MISMATCHES: [(&str, &str); 5] = [
    ("http://127.0.0.1:8080", "127.0.0.2"),
    ("http://127.0.0.1:8080", "::1"),
    ("http://[::1]:8080", "127.0.0.1"),
    ("http://[::1]:8080", "::2"),
    ("http://2130706433:8080", "127.0.0.2"),
];

/// A literal-origin exception names the literal's own address, and admits
/// that literal at that port only.
fn assert_literal_origins_match_their_address() {
    for (origin, address) in LITERAL_MISMATCHES {
        assert_exception_refused(restricted(), origin, ip(address), "literal mismatch");
    }
    for (origin, address) in LITERAL_MATCHES {
        let address = ip(address);
        let policy = excepting(restricted(), origin, address);
        let target = format!("{origin}/");
        assert_eq!(
            verdict(&policy, &target, address),
            Verdict::Permitted,
            "{origin}"
        );
    }

    let loopback = ip("127.0.0.1");
    let policy = excepting(restricted(), "http://127.0.0.1:8080", loopback);
    for (target, address) in [
        ("http://127.0.0.1:8081/", "127.0.0.1"),
        ("http://127.0.0.2:8080/", "127.0.0.2"),
        ("https://127.0.0.1:8080/", "127.0.0.1"),
        ("http://[::1]:8080/", "::1"),
    ] {
        assert_eq!(
            verdict(&policy, target, ip(address)),
            Verdict::Address,
            "{target}"
        );
    }
}

/// A public literal target, an address it is asked at, and the verdict. Every
/// address here is one the restricted policy permits under a name.
const LITERAL_TARGETS: [(&str, &str, Verdict); 8] = [
    ("http://8.8.8.8/", "8.8.8.8", Verdict::Permitted),
    ("http://8.8.8.8/", "::ffff:8.8.8.8", Verdict::Permitted),
    ("http://[::ffff:8.8.8.8]/", "8.8.8.8", Verdict::Permitted),
    ("http://8.8.8.8/", "1.1.1.1", Verdict::Address),
    ("http://8.8.8.8/", "::ffff:1.1.1.1", Verdict::Address),
    ("http://8.8.8.8/", "2606:4700:4700::1111", Verdict::Address),
    (
        "http://[2606:4700:4700::1111]/",
        "2606:4700:4700::1001",
        Verdict::Address,
    ),
    (
        "http://[2606:4700:4700::1111]/",
        "8.8.8.8",
        Verdict::Address,
    ),
];

/// A literal target is permitted at its own address and refused at any other,
/// though a name is permitted at each of them.
fn assert_literal_targets_are_dialed_at_themselves() {
    let policy = restricted();
    for (target, address, expected) in LITERAL_TARGETS {
        let address = ip(address);
        assert_eq!(
            verdict(&policy, PUBLIC_NAME_URL, address),
            Verdict::Permitted,
            "a name that resolved to {address}"
        );
        assert_eq!(
            verdict(&policy, target, address),
            expected,
            "{target} at {address}"
        );
    }
}

/// The labels a generated name is built from. None is numeric, so no
/// generated name is an address.
const LABELS: [&str; 6] = ["svc", "billing", "internal", "corp", "edge-a", "x"];

/// The explicit ports a generated origin may carry. `None` is the default
/// port of its scheme.
const PORTS: [Option<u16>; 3] = [None, Some(8080), Some(8443)];

/// A port no generated origin is configured with.
const OTHER_PORT: u16 = 8081;

/// The class of target a generated case asks against its origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpellingClass {
    SameOrigin,
    AnotherPort,
    AnotherName,
    AnotherScheme,
}

impl SpellingClass {
    /// The name a run reports the class by.
    const fn label(self) -> &'static str {
        match self {
            Self::SameOrigin => "same origin",
            Self::AnotherPort => "another port",
            Self::AnotherName => "another name",
            Self::AnotherScheme => "another scheme",
        }
    }
}

/// The classes a generated case is drawn from.
const SPELLING_CLASSES: [SpellingClass; 4] = [
    SpellingClass::SameOrigin,
    SpellingClass::AnotherPort,
    SpellingClass::AnotherName,
    SpellingClass::AnotherScheme,
];

/// One canonical origin a generated case spells.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    secure: bool,
    host: Box<str>,
    port: Option<u16>,
}

impl Origin {
    /// The effective port, written out.
    fn effective_port(&self) -> u16 {
        match (self.port, self.secure) {
            (Some(port), _) => port,
            (None, true) => 443,
            (None, false) => 80,
        }
    }

    /// One spelling of this origin: a drawn letter case, root dot, port form,
    /// and root path.
    fn spell(&self, case: &mut DeterministicCase) -> Box<str> {
        let scheme = match (self.secure, case.boolean()) {
            (true, true) => "HTTPS",
            (true, false) => "https",
            (false, true) => "HTTP",
            (false, false) => "http",
        };
        let host: String = self
            .host
            .chars()
            .map(|letter| match case.boolean() {
                true => letter.to_ascii_uppercase(),
                false => letter,
            })
            .collect();
        let dot = match case.boolean() {
            true => ".",
            false => "",
        };
        let port = match (self.port, case.boolean()) {
            (None, false) => String::new(),
            (_, _) => format!(":{}", self.effective_port()),
        };
        let path = match case.boolean() {
            true => "/",
            false => "",
        };
        format!("{scheme}://{host}{dot}{port}{path}").into_boxed_str()
    }
}

/// One generated case: an exception, a target, and the verdict it must get.
#[derive(Debug, PartialEq, Eq)]
struct Spelling {
    class: SpellingClass,
    configured: Box<str>,
    requested: Box<str>,
    expected: Verdict,
}

/// Draw one origin, then a target of one class against it. `None` is the
/// origin itself, spelled again.
fn spelling(case: &mut DeterministicCase) -> Spelling {
    let labels = 1 + case.below(3);
    let host = (0..labels)
        .map(|_| *case.pick(&LABELS))
        .collect::<Vec<&str>>()
        .join(".")
        .into_boxed_str();
    let origin = Origin {
        secure: case.boolean(),
        host,
        port: *case.pick(&PORTS),
    };
    let class = *case.pick(&SPELLING_CLASSES);
    let (target, expected) = match class {
        SpellingClass::SameOrigin => (None, Verdict::Permitted),
        SpellingClass::AnotherPort => (
            Some(Origin {
                port: Some(OTHER_PORT),
                ..origin.clone()
            }),
            Verdict::Address,
        ),
        SpellingClass::AnotherName => (
            Some(Origin {
                secure: origin.secure,
                host: format!("{}.{}", case.pick(&LABELS), origin.host).into_boxed_str(),
                port: origin.port,
            }),
            Verdict::Address,
        ),
        SpellingClass::AnotherScheme => (
            Some(Origin {
                secure: !origin.secure,
                ..origin.clone()
            }),
            Verdict::Address,
        ),
    };
    Spelling {
        class,
        configured: origin.spell(case),
        requested: target.as_ref().unwrap_or(&origin).spell(case),
        expected,
    }
}

/// The generated cases one run asks.
const SPELLING_CASES: u64 = 512;

/// Generated spellings of one origin are that origin, and a generated target
/// with another port, name, or scheme is not.
fn assert_generated_spellings() {
    let generator = DeterministicGenerator::stable();
    let address = ip(EXCEPTED);
    let mut reached = BTreeSet::new();
    for index in 0..SPELLING_CASES {
        let (case, row) = generator.reproducible(index, spelling);
        let policy = excepting(restricted(), &row.configured, address);
        assert_eq!(
            verdict(&policy, &row.requested, address),
            row.expected,
            "{case}: {row:?}"
        );
        reached.insert(row.class.label());
    }
    generator.assert_reached(
        SPELLING_CASES,
        SPELLING_CLASSES.map(SpellingClass::label),
        &reached,
    );
}

/// 1.T1
#[test]
fn egress_policy_normalization_and_addresses_are_exact() {
    assert_unrestricted_permits_private_destinations();

    let policy = restricted();
    assert_intervals_are_exact(&policy, &V4_REFUSED);
    assert_intervals_are_exact(&policy, &V6_REFUSED);
    assert_ipv4_middles_are_refused(&policy);
    assert_named_addresses(&policy);
    assert_numeric_ipv4_is_canonical();
    assert_zones_are_refused();

    assert_origin_spellings_are_one_origin();
    assert_origin_inputs_are_refused();
    assert_exceptions_are_exact();
    assert_duplicates_are_idempotent();
    assert_https_only_is_not_weakened();
    assert_unrestricted_takes_no_exception();
    assert_unexceptable_addresses();
    assert_literal_origins_match_their_address();
    assert_literal_targets_are_dialed_at_themselves();
    assert_generated_spellings();
}
