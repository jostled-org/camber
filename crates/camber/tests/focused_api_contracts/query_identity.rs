//! Raw and decoded query identity on the public owned `Request`.
//!
//! Entered through `Request::builder`, so the parser's whole input space is
//! covered without booting a server per row. The wire-exact spelling and the
//! head-only and streaming-proxy construction paths are proved by
//! `component_http_routing::query_parameters`.

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
