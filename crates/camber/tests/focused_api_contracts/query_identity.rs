//! Raw and decoded query identity on the public owned `Request`.
//!
//! Entered through `Request::builder`, so the parser's whole input space is
//! covered without booting a server per row. The wire-exact spelling and the
//! head-only and streaming-proxy construction paths are proved by
//! `component_http_routing::query_parameters`.
//!
//! The generated table builds each target from labeled wire fragments whose
//! decoded text is written out beside them, so no expectation is computed by
//! the decoder under test.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use crate::deterministic::{DeterministicCase, DeterministicGenerator};
use camber::http::Request;

/// The request the row under test observes.
///
/// Every fixture target here is one Hyper's URI parser accepts, so a failure to
/// build is a broken row rather than a refused request.
fn request_for(target: &str) -> Request {
    Request::builder()
        .path(target)
        .finish()
        .expect("the fixture target is an accepted request target")
}

/// The decoded pairs a target yields, in wire order.
///
/// Collected only here: `query_pairs` borrows from the request's own cache, and
/// the assertion needs one comparable value rather than a live iterator.
fn pairs_of(request: &Request) -> Box<[(&str, &str)]> {
    request.query_pairs().collect()
}

fn assert_pairs(target: &str, expected: &[(&str, &str)]) {
    let request = request_for(target);
    assert_eq!(
        pairs_of(&request).as_ref(),
        expected,
        "decoded pairs for {target}"
    );
}

#[test]
fn raw_query_distinguishes_absent_and_explicit_empty() {
    assert_eq!(request_for("/items").raw_query(), None);
    assert_eq!(request_for("/items?").raw_query(), Some(""));
    assert_eq!(
        request_for("/items?x=%2f+%20").raw_query(),
        Some("x=%2f+%20"),
        "raw identity is the accepted spelling, not a decoded or normalized one"
    );
}

#[test]
fn query_pairs_preserve_order_duplicates_and_blank_components() {
    assert_pairs("/items", &[]);
    assert_pairs("/items?", &[]);
    assert_pairs(
        "/items?tag=a&tag=b&a.b.c=1&=blank&=&bare&name=",
        &[
            ("tag", "a"),
            ("tag", "b"),
            ("a.b.c", "1"),
            ("", "blank"),
            ("", ""),
            ("bare", ""),
            ("name", ""),
        ],
    );
    assert_pairs("/items?&a=1&&b=2&", &[("a", "1"), ("b", "2")]);
}

#[test]
fn query_pairs_split_before_decoding_and_decode_permissively() {
    assert_pairs("/items?a=1%262&b%3Dc=3", &[("a", "1&2"), ("b=c", "3")]);
    assert_pairs(
        "/items?sp=a+b&pct=a%20b&plus=%2B&mix=a+b%2Bc",
        &[
            ("sp", "a b"),
            ("pct", "a b"),
            ("plus", "+"),
            ("mix", "a b+c"),
        ],
    );
    assert_pairs("/items?up=%2F&low=%2f", &[("up", "/"), ("low", "/")]);
    assert_pairs("/items?check=%E2%9C%93", &[("check", "\u{2713}")]);
    assert_pairs(
        "/items?bad=%zz&short=%4&trail=%",
        &[("bad", "%zz"), ("short", "%4"), ("trail", "%")],
    );
    assert_pairs("/items?invalid=%FF", &[("invalid", "\u{FFFD}")]);

    let escaped_delimiters = request_for("/items?a=1%262&b%3Dc=3");
    assert_eq!(
        escaped_delimiters.query_pairs().count(),
        2,
        "an escaped delimiter cannot open a new pair or key boundary"
    );
}

#[test]
fn keyed_helpers_keep_nonempty_lookup_contract_when_pairs_expose_blank_keys() {
    let request = request_for("/items?=blank&tag=a&=&tag=b");

    assert_eq!(request.query("tag"), Some("a"));
    assert_eq!(request.query_all("tag").collect::<Vec<_>>(), ["a", "b"]);
    assert_eq!(request.query(""), None);
    assert_eq!(request.query_all("").count(), 0);
    assert_eq!(
        pairs_of(&request).as_ref(),
        &[("", "blank"), ("tag", "a"), ("", ""), ("tag", "b")],
        "the keyed guard hides blank keys from lookup, not from iteration"
    );
}

#[test]
fn query_accessors_share_one_cached_pair_sequence() {
    let request = request_for("/items?tag=a&tag=b");

    let from_query = request.query("tag").expect("the first tag value");
    let from_all = request
        .query_all("tag")
        .next()
        .expect("the first tag value again");
    let (pair_key, pair_value) = request
        .query_pairs()
        .next()
        .expect("the first decoded pair is the first tag pair");

    assert!(
        std::ptr::eq(from_query, from_all),
        "query and query_all read one stored value"
    );
    assert!(
        std::ptr::eq(from_query, pair_value),
        "query_pairs reads that same stored value"
    );

    let (repeat_key, repeat_value) = request
        .query_pairs()
        .next()
        .expect("repeated iteration reaches the same first pair");
    assert!(
        std::ptr::eq(pair_key, repeat_key),
        "a second iteration reads the same stored key"
    );
    assert!(
        std::ptr::eq(pair_value, repeat_value),
        "a second iteration reads the same stored value"
    );
}

/// What the accessors cost, measured rather than read off the source.
///
/// `allocation-counter` owns the counting `GlobalAlloc`, so it is referenced
/// only when Camber leaves the process allocator alone: `jemalloc` and
/// `mimalloc` each install their own, and two global allocators do not link.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
#[test]
fn query_accessors_obey_allocation_contract() {
    let request = request_for("/items?tag=a&tag=b&note=a%20b");

    let calibration = allocation_counter::measure(|| {
        drop(std::hint::black_box(Box::new(1_u32)));
    });
    assert!(
        calibration.count_total > 0,
        "a probe that counts nothing would make every zero below meaningless"
    );

    let raw = allocation_counter::measure(|| {
        std::hint::black_box(request.raw_query());
    });
    assert_eq!(
        raw.count_total, 0,
        "raw_query borrows the accepted target and allocates nothing"
    );

    let cold = allocation_counter::measure(|| {
        request.query_pairs().for_each(|pair| {
            std::hint::black_box(pair);
        });
    });
    assert!(
        cold.count_total > 0,
        "raw access left the decoded cache cold, so this first pass pays to fill it"
    );

    let warm = allocation_counter::measure(|| {
        std::hint::black_box(request.query("tag"));
        request.query_all("tag").for_each(|value| {
            std::hint::black_box(value);
        });
        request.query_pairs().for_each(|pair| {
            std::hint::black_box(pair);
        });
    });
    assert_eq!(
        warm.count_total, 0,
        "an initialized decoded sequence is borrowed, never rebuilt or copied"
    );
}

#[test]
fn form_blank_keys_remain_filtered_after_query_admission_changes() {
    let request = Request::builder()
        .method("POST")
        .expect("POST is an accepted method")
        .path("/submit")
        .header("content-type", "application/x-www-form-urlencoded")
        .body("=hidden&&name=value%20here&")
        .finish()
        .expect("the fixture form request is well formed");

    assert_eq!(
        request.form(""),
        None,
        "form admission still rejects a blank field name"
    );
    assert_eq!(request.form("name"), Some("value here"));
}

/// The checked-in seed every generated query case derives from.
const QUERY_PROPERTY_SEED: u64 = 0x5155_4552_5900_000a;
const GENERATED_QUERY_CASES: u64 = 128;
const MAX_QUERY_BYTES: usize = 64 * 1024;
/// Draws `0..7` segments, so an explicit empty query is reachable.
const SEGMENT_BOUND: NonZeroUsize = NonZeroUsize::new(7).unwrap();
/// Draws `0..4` fragments, so a blank key or value is reachable.
const FRAGMENT_BOUND: NonZeroUsize = NonZeroUsize::new(4).unwrap();
/// One case in eight carries no `?` at all.
const ABSENT_QUERY_ONE_IN: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// Names drawn often enough that duplicate keys arise in most cases.
const REPEATED_KEYS: [&str; 2] = ["tag", "a.b"];

/// Where a grammar fragment may appear inside one component.
#[derive(Clone, Copy)]
enum Placement {
    Anywhere,
    /// A literal `=` is data only after the first one has split the segment.
    ValueOnly,
    /// An incomplete escape stays literal only while nothing follows it; a
    /// hex digit after `%4` would complete it.
    Trailing,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    Key,
    Value,
}

impl Placement {
    fn admits(self, role: Role, last: bool) -> bool {
        match self {
            Self::Anywhere => true,
            Self::ValueOnly => role == Role::Value,
            Self::Trailing => last,
        }
    }
}

/// One labeled wire spelling and the text the documented rules decode it to.
///
/// The decoded column is written by hand from `docs/reference/http.md`, never
/// computed by Camber's decoder.
struct Fragment {
    label: &'static str,
    wire: &'static str,
    decoded: &'static str,
    placement: Placement,
}

const fn fragment(
    label: &'static str,
    wire: &'static str,
    decoded: &'static str,
    placement: Placement,
) -> Fragment {
    Fragment {
        label,
        wire,
        decoded,
        placement,
    }
}

/// No fragment decodes to a leading UTF-8 continuation byte, so a truncated
/// or invalid byte next to any other fragment replaces exactly itself.
const FRAGMENTS: [Fragment; 19] = [
    fragment("plain", "tag", "tag", Placement::Anywhere),
    fragment("dotted", "a.b", "a.b", Placement::Anywhere),
    fragment("unreserved", "x-_~9", "x-_~9", Placement::Anywhere),
    fragment("plus", "+", " ", Placement::Anywhere),
    fragment("escaped-space", "%20", " ", Placement::Anywhere),
    fragment("escaped-plus", "%2B", "+", Placement::Anywhere),
    fragment("escaped-ampersand", "%26", "&", Placement::Anywhere),
    fragment("escaped-equals-upper", "%3D", "=", Placement::Anywhere),
    fragment("escaped-equals-lower", "%3d", "=", Placement::Anywhere),
    fragment("escaped-slash-lower", "%2f", "/", Placement::Anywhere),
    fragment("escaped-percent", "%25", "%", Placement::Anywhere),
    fragment(
        "utf8-escape-upper",
        "%E2%9C%93",
        "\u{2713}",
        Placement::Anywhere,
    ),
    fragment("utf8-escape-lower", "%c3%a9", "\u{e9}", Placement::Anywhere),
    fragment("malformed-escape", "%zz", "%zz", Placement::Anywhere),
    fragment("invalid-utf8", "%FF", "\u{FFFD}", Placement::Anywhere),
    fragment("truncated-utf8", "%C3", "\u{FFFD}", Placement::Anywhere),
    fragment("literal-equals", "=", "=", Placement::ValueOnly),
    fragment("short-escape", "%4", "%4", Placement::Trailing),
    fragment("bare-percent", "%", "%", Placement::Trailing),
];

/// One component's wire spelling beside its independently decoded text.
#[derive(Default)]
struct Component {
    wire: String,
    decoded: String,
}

/// One `&`-delimited segment and the pair it must yield, if any.
struct Segment {
    wire: Box<str>,
    pair: Option<(Box<str>, Box<str>)>,
}

/// A generated request target and everything the accessors must answer.
#[derive(Debug, PartialEq)]
struct QueryCase {
    category: &'static str,
    target: Box<str>,
    raw: Option<Box<str>>,
    pairs: Box<[(Box<str>, Box<str>)]>,
    labels: Box<[&'static str]>,
}

fn generated_component(
    case: &mut DeterministicCase,
    role: Role,
    labels: &mut Vec<&'static str>,
) -> Component {
    let count = case.bounded(FRAGMENT_BOUND);
    let mut component = Component::default();
    for position in 0..count {
        let last = position + 1 == count;
        let mut admitted = FRAGMENTS
            .iter()
            .filter(|fragment| fragment.placement.admits(role, last));
        let drawn = case.below(admitted.clone().count());
        let fragment = admitted
            .nth(drawn)
            .expect("a drawn index lies inside the admitted fragments");
        component.wire.push_str(fragment.wire);
        component.decoded.push_str(fragment.decoded);
        labels.push(fragment.label);
    }
    component
}

fn generated_key(case: &mut DeterministicCase, labels: &mut Vec<&'static str>) -> Component {
    match case.boolean() {
        true => {
            let key = *case.pick(&REPEATED_KEYS);
            labels.push("repeated-key");
            Component {
                wire: key.to_owned(),
                decoded: key.to_owned(),
            }
        }
        false => generated_component(case, Role::Key, labels),
    }
}

fn assigned_segment(key: Component, value: Component, labels: &mut Vec<&'static str>) -> Segment {
    labels.push(match key.wire.is_empty() {
        true => "blank-key",
        false => "assigned",
    });
    Segment {
        wire: format!("{}={}", key.wire, value.wire).into(),
        pair: Some((key.decoded.into(), value.decoded.into())),
    }
}

/// A segment without `=`: a named one has a blank value, an empty one is no
/// pair at all.
fn bare_segment(key: Component, labels: &mut Vec<&'static str>) -> Segment {
    match key.wire.is_empty() {
        true => {
            labels.push("empty-segment");
            Segment {
                wire: "".into(),
                pair: None,
            }
        }
        false => {
            labels.push("bare-key");
            Segment {
                wire: key.wire.into(),
                pair: Some((key.decoded.into(), "".into())),
            }
        }
    }
}

fn generated_segment(case: &mut DeterministicCase, labels: &mut Vec<&'static str>) -> Segment {
    let key = generated_key(case, labels);
    match case.boolean() {
        true => {
            let value = generated_component(case, Role::Value, labels);
            assigned_segment(key, value, labels)
        }
        false => bare_segment(key, labels),
    }
}

/// A target with a `?`, whose segments join with `&` in generated order.
///
/// Empty segments land wherever they were drawn, so leading, consecutive, and
/// trailing delimiters all arise from the one rule.
fn present_query_case(case: &mut DeterministicCase) -> QueryCase {
    let mut labels = Vec::new();
    let segments: Box<[Segment]> = (0..case.bounded(SEGMENT_BOUND))
        .map(|_| generated_segment(case, &mut labels))
        .collect();
    let wire = segments
        .iter()
        .map(|segment| segment.wire.as_ref())
        .collect::<Box<[_]>>()
        .join("&");
    let category = match wire.is_empty() {
        true => "empty-query",
        false => "pairs",
    };
    labels.push(category);
    QueryCase {
        category,
        target: format!("/items?{wire}").into(),
        raw: Some(wire.into()),
        pairs: segments
            .into_iter()
            .filter_map(|segment| segment.pair)
            .collect(),
        labels: labels.into_boxed_slice(),
    }
}

fn generated_query_case(case: &mut DeterministicCase) -> QueryCase {
    match case.bounded(ABSENT_QUERY_ONE_IN) {
        0 => QueryCase {
            category: "absent-query",
            target: "/items".into(),
            raw: None,
            pairs: Box::new([]),
            labels: Box::new(["absent-query"]),
        },
        _ => present_query_case(case),
    }
}

/// Every keyed answer the independent pair list implies.
///
/// A non-empty name answers its values in pair order; the empty name answers
/// nothing even when blank keys are present.
fn assert_keyed_lookups(request: &Request, expected: &QueryCase, context: &str) {
    for (name, _) in expected.pairs.iter().filter(|(name, _)| !name.is_empty()) {
        let values: Box<[&str]> = expected
            .pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref())
            .collect();
        assert_eq!(
            request.query_all(name).collect::<Box<[_]>>(),
            values,
            "{context}: query_all keeps every duplicate in wire order"
        );
        assert_eq!(
            request.query(name),
            values.first().copied(),
            "{context}: query answers the first duplicate"
        );
    }
    assert_eq!(request.query(""), None, "{context}: blank name lookup");
    assert_eq!(
        request.query_all("").count(),
        0,
        "{context}: blank name values"
    );
}

fn assert_query_case(case: &DeterministicCase, expected: &QueryCase) {
    let context = format!(
        "{case} category={} target={}",
        expected.category, expected.target
    );
    assert!(
        expected.target.len() <= MAX_QUERY_BYTES,
        "{context}: generated target exceeds the query bound"
    );
    let request = request_for(&expected.target);
    let expected_pairs: Box<[(&str, &str)]> = expected
        .pairs
        .iter()
        .map(|(key, value)| (key.as_ref(), value.as_ref()))
        .collect();

    assert_eq!(
        request.raw_query(),
        expected.raw.as_deref(),
        "{context}: raw spelling"
    );
    assert_eq!(
        pairs_of(&request),
        expected_pairs,
        "{context}: pairs split before decoding, once each"
    );
    assert_keyed_lookups(&request, expected, context.as_str());
}

fn has_duplicate_key(pairs: &[(Box<str>, Box<str>)]) -> bool {
    pairs
        .iter()
        .enumerate()
        .any(|(index, (key, _))| pairs[index + 1..].iter().any(|(later, _)| later == key))
}

/// Labels a run must reach beyond the fragment table, or the property proves
/// less than it claims.
const REQUIRED_SHAPES: [&str; 9] = [
    "absent-query",
    "duplicate-key",
    "empty-query",
    "pairs",
    "assigned",
    "blank-key",
    "bare-key",
    "empty-segment",
    "repeated-key",
];

#[test]
fn generated_query_pairs_preserve_raw_identity_and_decode_once() {
    let generator = DeterministicGenerator::new(QUERY_PROPERTY_SEED);
    let mut reached = BTreeSet::new();

    for index in 0..GENERATED_QUERY_CASES {
        let (case, expected) = generator.reproducible(index, generated_query_case);
        assert_eq!(case.seed(), QUERY_PROPERTY_SEED, "{case}: checked-in seed");
        assert_query_case(&case, &expected);
        reached.extend(expected.labels.iter().copied());
        if has_duplicate_key(&expected.pairs) {
            reached.insert("duplicate-key");
        }
    }

    generator.assert_reached(
        GENERATED_QUERY_CASES,
        FRAGMENTS
            .iter()
            .map(|fragment| fragment.label)
            .chain(REQUIRED_SHAPES),
        &reached,
    );
}
