//! 4.T1 and 4.T3, with the typed half of outbound 5.T1: the pure decisions one
//! retried call is delayed and replayed by.
//!
//! Every helper here is the production function itself, reached through the
//! doc-hidden `__private` surface. The wall clock is an input and so is the
//! jitter sample, so every row states its exact answer rather than a range an
//! elapsed clock or a random draw happened to land in. The replay decision
//! reads a real Reqwest failure, a method, and the request's own permission.

use crate::client_failures;
use crate::deterministic::DeterministicGenerator;

use camber::__private::{
    client_error_projection, client_retry_after_delay, client_retry_backoff,
    client_retryable_transport,
};
use camber::http::ClientError;
use camber::http::ClientRedirect;
use camber::http::ClientTimeout;
use camber::http::RetryPolicy;
use reqwest::Method;
use std::collections::BTreeSet;
use std::io::Read;
use std::num::NonZeroUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The fixed wall time every `Retry-After` row is read at:
/// `Sun, 06 Nov 1994 08:49:37 GMT`, the RFC 9110 example instant.
const NOW_UNIX: u64 = 784_111_777;

/// [`NOW_UNIX`] written as an IMF-fixdate.
const NOW_FIXDATE: &str = "Sun, 06 Nov 1994 08:49:37 GMT";

/// The latest instant an IMF-fixdate can name: its four-digit year ends at
/// 9999.
const LATEST_FIXDATE: &str = "Fri, 31 Dec 9999 23:59:59 GMT";
const LATEST_FIXDATE_UNIX: u64 = 253_402_300_799;

/// The largest delta-seconds value a `u64` second count holds, and the first
/// value past it.
const MAX_DELTA: &str = "18446744073709551615";
const OVERFLOWING_DELTA: &str = "18446744073709551616";

/// Checked-in seeds, one per generated family, so a failing case is replayed
/// from its seed and index alone.
const DELTA_SEED: u64 = 0x5245_5452_5944_0401;
const FIXDATE_SEED: u64 = 0x5245_5452_5944_0402;
const BACKOFF_SEED: u64 = 0x5245_5452_5944_0403;

/// The cap on generated cases per family.
const GENERATED_CASES: u64 = 128;

/// How far a generated fixdate row reaches from [`NOW_UNIX`] in either
/// direction. Below `NOW_UNIX`, so a past row never names a year before 1970.
const FIXDATE_REACH: NonZeroUsize = NonZeroUsize::new(700_000_000).unwrap();

/// The largest base a generated backoff row draws, in nanoseconds.
const BACKOFF_BASE_REACH: NonZeroUsize = NonZeroUsize::new(10_000_000_001).unwrap();

/// Every attempt index whose multiplier `2^attempt` a `u32` holds.
const EXACT_ATTEMPTS: NonZeroUsize = NonZeroUsize::new(32).unwrap();

/// The generated fixdate categories a run must reach, or the family proves
/// only one side of now.
const FIXDATE_CATEGORIES: [&str; 2] = ["future-fixdate", "past-fixdate"];

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW_UNIX)
}

/// The proleptic Gregorian date `days` after 1970-01-01.
///
/// Howard Hinnant's `civil_from_days`, restricted to the non-negative days a
/// fixdate can name. It is the row's own oracle for the text it writes, not a
/// copy of the parser under test.
fn civil_from_days(days: u64) -> (u64, usize, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = match shifted_month < 10 {
        true => shifted_month + 3,
        false => shifted_month - 9,
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month as usize, day)
}

/// The IMF-fixdate naming `unix` seconds after the epoch.
fn imf_fixdate(unix: u64) -> String {
    let days = unix / 86_400;
    let seconds = unix % 86_400;
    // 1970-01-01 was a Thursday.
    let weekday = WEEKDAYS[((days + 4) % 7) as usize];
    let (year, month, day) = civil_from_days(days);
    format!(
        "{weekday}, {day:02} {} {year:04} {:02}:{:02}:{:02} GMT",
        MONTHS[month - 1],
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60,
    )
}

/// The oracle agrees with both fixed instants before any generated row trusts
/// it.
fn assert_oracle_agrees() {
    assert_eq!(imf_fixdate(NOW_UNIX), NOW_FIXDATE, "fixdate oracle at now");
    assert_eq!(
        imf_fixdate(LATEST_FIXDATE_UNIX),
        LATEST_FIXDATE,
        "fixdate oracle at the latest fixdate"
    );
}

fn assert_retry_after(value: &str, expected: Option<Duration>, label: &str) {
    assert_eq!(
        client_retry_after_delay(value, now()),
        expected,
        "{label}: Retry-After {value:?} read at {NOW_FIXDATE}"
    );
}

/// Fixed delta-seconds and IMF-fixdate rows, each with its exact delay.
fn assert_fixed_retry_after_rows() {
    let rows: [(&str, Option<Duration>, &str); 9] = [
        ("0", Some(Duration::ZERO), "zero delta"),
        ("120", Some(Duration::from_secs(120)), "delta"),
        (
            "007",
            Some(Duration::from_secs(7)),
            "delta with leading zeros",
        ),
        (
            MAX_DELTA,
            Some(Duration::from_secs(u64::MAX)),
            "maximum parseable delta",
        ),
        (NOW_FIXDATE, Some(Duration::ZERO), "fixdate equal to now"),
        (
            "Sun, 06 Nov 1994 08:49:36 GMT",
            Some(Duration::ZERO),
            "fixdate one second past",
        ),
        (
            "Sun, 06 Nov 1994 08:51:37 GMT",
            Some(Duration::from_secs(120)),
            "fixdate two minutes ahead",
        ),
        (
            "Thu, 01 Jan 1970 00:00:00 GMT",
            Some(Duration::ZERO),
            "fixdate at the epoch",
        ),
        (
            LATEST_FIXDATE,
            Some(Duration::from_secs(LATEST_FIXDATE_UNIX - NOW_UNIX)),
            "latest fixdate",
        ),
    ];
    for (value, expected, label) in rows {
        assert_retry_after(value, expected, label);
    }
}

/// Values outside both forms: each is refused, so the caller keeps its
/// configured backoff rather than reading one as a zero wait.
fn assert_invalid_retry_after_rows() {
    let rows: [(&str, &str); 16] = [
        ("", "empty"),
        ("-1", "negative delta"),
        ("+1", "signed delta"),
        ("1.5", "fractional delta"),
        ("5s", "delta with a unit"),
        ("0x10", "hexadecimal delta"),
        ("soon", "text"),
        (OVERFLOWING_DELTA, "delta past the largest second count"),
        ("Sun, 31 Nov 1994 08:49:37 GMT", "day past the month's end"),
        (
            "Mon, 06 Nov 1994 08:49:37 GMT",
            "weekday disagreeing with the date",
        ),
        ("Sun, 06 Nov 1994 24:00:00 GMT", "hour past the day"),
        ("Sun, 06 Nov 1994 08:60:37 GMT", "minute past the hour"),
        ("Sun, 06 Foo 1994 08:49:37 GMT", "unknown month"),
        ("Sun, 06 Nov 1994 08:49:37 PST", "zone other than GMT"),
        ("Sun, 06 Nov 1994 08:49:37", "missing zone"),
        ("Sun, 06 Nov 10000 00:00:00 GMT", "five-digit year"),
    ];
    for (value, label) in rows {
        assert_retry_after(value, None, label);
    }
}

/// Generated delta-seconds rows: every `u64` second count, with or without
/// leading zeros, is exactly that many seconds.
fn assert_generated_delta_rows() {
    let generator = DeterministicGenerator::new(DELTA_SEED);
    for index in 0..GENERATED_CASES {
        let mut case = generator.case(index);
        let seconds = case.bounded(NonZeroUsize::MAX) as u64;
        let padding = case.below(4);
        let value = format!("{}{seconds}", "0".repeat(padding));
        assert_retry_after(
            &value,
            Some(Duration::from_secs(seconds)),
            &format!("{case} category=delta-seconds"),
        );
    }
}

/// Generated IMF-fixdate rows on both sides of now: a future date is exactly
/// its distance away, and a past date is no wait at all.
fn assert_generated_fixdate_rows() {
    let generator = DeterministicGenerator::new(FIXDATE_SEED);
    let mut reached = BTreeSet::new();
    for index in 0..GENERATED_CASES {
        let mut case = generator.case(index);
        let offset = case.bounded(FIXDATE_REACH) as u64;
        let (unix, expected, category) = match case.boolean() {
            true => (
                NOW_UNIX + offset,
                Duration::from_secs(offset),
                "future-fixdate",
            ),
            false => (NOW_UNIX - offset, Duration::ZERO, "past-fixdate"),
        };
        assert_retry_after(
            &imf_fixdate(unix),
            Some(expected),
            &format!("{case} category={category}"),
        );
        reached.insert(category);
    }
    generator.assert_reached(GENERATED_CASES, FIXDATE_CATEGORIES, &reached);
}

/// 4.T1
#[test]
fn retry_delay_grammar_and_saturation() {
    assert_oracle_agrees();
    assert_fixed_retry_after_rows();
    assert_invalid_retry_after_rows();
    assert_generated_delta_rows();
    assert_generated_fixdate_rows();
    assert_fixed_backoff_rows();
    assert_saturated_backoff_rows();
    assert_generated_backoff_rows();
}

/// The delay the documented formula names: `base · 2^attempt`, plus the jitter
/// sample reduced below `base`, every step saturating.
///
/// Stated only for the attempts whose multiplier a `u32` holds; past those the
/// rows assert saturation, not a multiplier.
fn exact_backoff(base: Duration, attempt: u32, jitter: u64) -> Duration {
    let bound = base.as_nanos().min(u128::from(u64::MAX)) as u64;
    let reduced = match bound {
        0 => Duration::ZERO,
        bound => Duration::from_nanos(jitter % bound),
    };
    base.saturating_mul(1_u32 << attempt)
        .saturating_add(reduced)
}

fn assert_backoff(base: Duration, attempt: u32, jitter: u64, expected: Duration, label: &str) {
    assert_eq!(
        client_retry_backoff(base, attempt, jitter),
        expected,
        "{label}: base {base:?}, attempt {attempt}, jitter sample {jitter}"
    );
}

/// The base the fixed backoff rows share.
const SECOND: Duration = Duration::from_secs(1);

fn assert_fixed_backoff_rows() {
    const SECOND_NANOS: u64 = 1_000_000_000;

    assert_backoff(SECOND, 0, 0, SECOND, "first attempt, no jitter");
    assert_backoff(
        SECOND,
        0,
        SECOND_NANOS - 1,
        2 * SECOND - Duration::from_nanos(1),
        "jitter just below the base",
    );
    assert_backoff(
        SECOND,
        0,
        SECOND_NANOS,
        SECOND,
        "jitter reduced below the base",
    );
    assert_backoff(SECOND, 1, 0, 2 * SECOND, "second attempt doubles");
    assert_backoff(
        SECOND,
        31,
        0,
        SECOND * (1_u32 << 31),
        "largest exact multiplier",
    );
    for attempt in [0, 31, 32, u32::MAX] {
        assert_backoff(
            Duration::ZERO,
            attempt,
            u64::MAX,
            Duration::ZERO,
            "zero base",
        );
    }
}

/// Past the exact multipliers the delay neither wraps nor shrinks, and a base
/// at the clock's limit stays at the limit.
fn assert_saturated_backoff_rows() {
    let largest_exact = client_retry_backoff(SECOND, 31, 0);
    for attempt in [32, u32::MAX] {
        let saturated = client_retry_backoff(SECOND, attempt, 0);
        assert!(
            saturated >= largest_exact,
            "attempt {attempt}: {saturated:?} fell below attempt 31's {largest_exact:?}"
        );
    }
    assert_eq!(
        client_retry_backoff(SECOND, 32, 0),
        client_retry_backoff(SECOND, u32::MAX, 0),
        "the multiplier did not saturate by attempt 32"
    );
    for attempt in [0, 1, 31, 32, u32::MAX] {
        for jitter in [0, 1, u64::MAX] {
            assert_backoff(
                Duration::MAX,
                attempt,
                jitter,
                Duration::MAX,
                "base at the clock's limit",
            );
        }
    }
}

/// Generated backoff rows over the exact attempts: the delay is the formula's,
/// and the jitter it adds stays below the base.
fn assert_generated_backoff_rows() {
    let generator = DeterministicGenerator::new(BACKOFF_SEED);
    for index in 0..GENERATED_CASES {
        let mut case = generator.case(index);
        let base = Duration::from_nanos(case.bounded(BACKOFF_BASE_REACH) as u64);
        let attempt = case.bounded(EXACT_ATTEMPTS) as u32;
        let jitter = case.bounded(NonZeroUsize::MAX) as u64;
        let label = format!("{case} category=backoff");
        let delay = client_retry_backoff(base, attempt, jitter);
        let floor = base.saturating_mul(1_u32 << attempt);
        assert!(
            delay >= floor && (base.is_zero() || delay - floor < base),
            "{label}: {delay:?} is outside [{floor:?}, {floor:?} + {base:?})"
        );
        assert_backoff(
            base,
            attempt,
            jitter,
            exact_backoff(base, attempt, jitter),
            &label,
        );
    }
}

/// How many leading [`methods`] are idempotent.
const IDEMPOTENT_METHOD_COUNT: usize = 5;

/// Every method the public client offers, the idempotent ones first, then an
/// extension token.
fn methods() -> [Method; 8] {
    [
        Method::GET,
        Method::HEAD,
        Method::OPTIONS,
        Method::PUT,
        Method::DELETE,
        Method::POST,
        Method::PATCH,
        Method::from_bytes(b"PURGE").unwrap(),
    ]
}

/// The path each failing send names. A raw failure prints its URL, so the
/// projection rows in `client_failures` read these as secrets.
pub(crate) const REFUSED_PATH_SECRET: &str = "refused-path-sentinel-6b1f";
pub(crate) const AMBIGUOUS_PATH_SECRET: &str = "ambiguous-path-sentinel-d470";
pub(crate) const UNTRUSTED_PATH_SECRET: &str = "untrusted-path-sentinel-93ce";

/// The origin no service listens on: port zero is never a listener's address.
const REFUSED_ORIGIN: &str = "http://127.0.0.1:0";

/// A plain Reqwest client with its own resend policy off, so each error below
/// is the first and only send's.
fn direct_client() -> reqwest::Client {
    reqwest::Client::builder()
        .retry(reqwest::retry::never())
        .build()
        .unwrap()
}

/// The error a send reported; an answer means the row's fixture failed.
fn send_error(sent: reqwest::Result<reqwest::Response>, answered: &str) -> reqwest::Error {
    match sent {
        Err(error) => error,
        Ok(response) => panic!("{answered} {}", response.status()),
    }
}

/// The error a send to a refused loopback port reports.
pub(crate) async fn refused_connect_error() -> reqwest::Error {
    let sent = direct_client()
        .post(format!("{REFUSED_ORIGIN}/{REFUSED_PATH_SECRET}"))
        .body("post")
        .send()
        .await;
    send_error(sent, "a refused port answered")
}

/// The real-time bound on the ambiguous peer's head read and on the untrusted
/// peer's handshake. It bounds fixture failure only.
const PEER_HEAD_BOUND: Duration = Duration::from_secs(10);

/// Read one request head and a body prefix, then close without answering.
///
/// The request must arrive, or the error the row classifies is not the one
/// an accepted, unanswered send reports. It blocks, so it runs off the row's
/// runtime.
fn read_then_close(listener: std::net::TcpListener) {
    let (mut stream, _) = listener.accept().unwrap();
    if let Err(error) = crate::http::read_head(&mut stream, PEER_HEAD_BOUND) {
        panic!("the client closed before its request head ended: {error}");
    }
    let mut byte = [0_u8; 1];
    let read = stream.read(&mut byte).unwrap();
    assert_eq!(read, 1, "the client closed before its request body began");
    drop(stream);
}

/// The error a send reports when its accepted request is closed unanswered.
pub(crate) async fn ambiguous_transport_error() -> reqwest::Error {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::task::spawn_blocking(move || read_then_close(listener));
    let sent = direct_client()
        .post(format!("http://{addr}/{AMBIGUOUS_PATH_SECRET}"))
        .body("post")
        .send()
        .await;
    peer.await.unwrap();
    crate::http::assert_address_reused(addr, "ambiguous transport peer").await;
    send_error(sent, "an unanswered request returned")
}

/// The error a send reports when the peer presents a certificate no trust
/// store holds.
///
/// The certificate names the loopback address the client dials, so only the
/// issuer is refused. The peer must see its handshake fail, or the error the
/// row classifies is not a validation failure.
pub(crate) async fn untrusted_tls_error() -> reqwest::Error {
    let (cert_pem, key_pem) = crate::tls_support::generate_cert_with_san("127.0.0.1");
    let acceptor = tokio_rustls::TlsAcceptor::from(crate::tls_support::build_server_config(
        &cert_pem, &key_pem,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        acceptor.accept(stream).await.is_err()
    });
    let sent = direct_client()
        .get(format!("https://{addr}/{UNTRUSTED_PATH_SECRET}"))
        .send()
        .await;
    let handshake_failed = match tokio::time::timeout(PEER_HEAD_BOUND, peer).await {
        Ok(joined) => joined.unwrap(),
        Err(_) => panic!("untrusted TLS peer: no handshake ended within {PEER_HEAD_BOUND:?}"),
    };
    crate::http::assert_address_reused(addr, "untrusted TLS peer").await;
    assert!(
        handshake_failed,
        "the client completed a handshake with a certificate nothing trusts"
    );
    send_error(sent, "an untrusted peer answered")
}

/// Every replay permission one decision is asked under.
const PERMISSIONS: [RetryPolicy; 3] = [
    RetryPolicy::Never,
    RetryPolicy::Idempotent,
    RetryPolicy::Replayable,
];

/// Whether `policy` lets `method` be sent again at all.
///
/// The row's own oracle. The match has no wildcard arm, so a new permission
/// fails to compile until its answer is written here.
fn permits(policy: RetryPolicy, method: &Method) -> bool {
    match policy {
        RetryPolicy::Never => false,
        RetryPolicy::Idempotent => methods()[..IDEMPOTENT_METHOD_COUNT].contains(method),
        RetryPolicy::Replayable => true,
    }
}

/// One failed send, by the category the production projection gives it.
///
/// A refused dial, a request closed unanswered, and an expired transport timer
/// are transient: another send may succeed, so a permitted request is sent
/// again. The client cannot tell how far the unanswered request got, so the
/// method's permission is the only authority for repeating it. A failure of
/// the description repeats itself. A name that does not resolve, a certificate
/// nothing trusts, and a spent redirect allowance are terminal under every
/// permission.
#[derive(Clone, Copy, Debug)]
enum Failure {
    Connect,
    Ambiguous,
    Builder,
    Dns,
    Tls,
    TransportTimeout,
    Redirect,
}

impl Failure {
    fn category(self) -> ClientError {
        match self {
            Self::Connect => ClientError::Connect,
            Self::Ambiguous => ClientError::Transport,
            Self::Builder => ClientError::Configuration,
            Self::Dns => ClientError::Dns,
            Self::Tls => ClientError::Tls,
            Self::TransportTimeout => ClientError::Timeout(ClientTimeout::Transport),
            Self::Redirect => ClientError::Redirect(ClientRedirect::HopLimit),
        }
    }

    fn transient(self) -> bool {
        match self {
            Self::Connect | Self::Ambiguous | Self::TransportTimeout => true,
            Self::Builder | Self::Dns | Self::Tls | Self::Redirect => false,
        }
    }
}

/// Require the fixture failure's category, then every method and permission's
/// decision for it.
fn assert_decisions(failure: Failure, error: &reqwest::Error) {
    assert_eq!(
        client_error_projection(error),
        failure.category(),
        "{failure:?} fixture failure was projected differently: {error:?}"
    );
    for method in &methods() {
        for policy in PERMISSIONS {
            assert_eq!(
                client_retryable_transport(error, method, policy),
                failure.transient() && permits(policy, method),
                "{failure:?} failure, {method} under {policy:?}"
            );
        }
    }
}

/// 4.T3, revised for request-local permission, and the typed half of outbound
/// 5.T1: the permission and the failure's category decide a repeat together.
///
/// Four failures come from fixtures that drive a runtime of their own, so they
/// are built before the row's runtime starts.
#[test]
fn replay_permission_decides_each_transport_failure() {
    let unresolved = client_failures::resolution_failure();
    let unbuilt = client_failures::target_failure();
    let timed_out = client_failures::connect_timeout_failure();
    let redirected = client_failures::redirect_failure();
    let (refused, ambiguous, untrusted) = client_failures::local_runtime().block_on(async {
        (
            refused_connect_error().await,
            ambiguous_transport_error().await,
            untrusted_tls_error().await,
        )
    });

    for (failure, error) in [
        (Failure::Connect, refused),
        (Failure::Ambiguous, ambiguous),
        (Failure::Builder, unbuilt),
        (Failure::Dns, unresolved),
        (Failure::Tls, untrusted),
        (Failure::TransportTimeout, timed_out),
        (Failure::Redirect, redirected),
    ] {
        assert_decisions(failure, &error);
    }
}
