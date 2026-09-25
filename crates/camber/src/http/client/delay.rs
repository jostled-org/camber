//! How long a retry sequence waits before its next attempt.
//!
//! Pure arithmetic over values: the wall clock a `Retry-After` date is read
//! against and the jitter sample a backoff adds are inputs, so every answer is
//! exact for its inputs. The delays are unclipped; the sequence that waits
//! clips each one to its own deadline.

use std::time::{Duration, SystemTime};

/// The wait one `Retry-After` value states, read at `now`.
///
/// Delta-seconds is one or more ASCII digits, leading zeros allowed; a count
/// past `u64::MAX` seconds names no wait. Any other value is read as an
/// HTTP-date, and a date at or before `now` states no wait at all. A value in
/// neither form is `None`, so the caller keeps its configured backoff rather
/// than reading the value as an immediate retry.
pub fn client_retry_after_delay(value: &str, now: SystemTime) -> Option<Duration> {
    match value.bytes().all(|byte| byte.is_ascii_digit()) {
        true => value.parse::<u64>().ok().map(Duration::from_secs),
        false => httpdate::parse_http_date(value)
            .ok()
            .map(|at| at.duration_since(now).unwrap_or(Duration::ZERO)),
    }
}

/// Exponential backoff after `attempt`, plus jitter below `base`.
///
/// The delay is `base · 2^attempt` plus `jitter` reduced below `base`. Every
/// step saturates: an attempt past the multipliers a `u32` holds uses
/// `u32::MAX`, and a product or sum past `Duration::MAX` is `Duration::MAX`.
pub fn client_retry_backoff(base: Duration, attempt: u32, jitter: u64) -> Duration {
    let multiplier = 1_u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let jitter_bound = u64::try_from(base.as_nanos()).unwrap_or(u64::MAX);
    let reduced = match jitter_bound {
        0 => Duration::ZERO,
        bound => Duration::from_nanos(jitter % bound),
    };
    base.saturating_mul(multiplier).saturating_add(reduced)
}

/// The delay before the attempt after `attempt`: the server's stated wait when
/// it gave a valid one, otherwise backoff with a fresh jitter sample.
pub(super) fn retry_delay(base: Duration, attempt: u32, stated: Option<Duration>) -> Duration {
    stated.unwrap_or_else(|| client_retry_backoff(base, attempt, crate::prng::next_u64()))
}

/// The wait a response's `Retry-After` header states, read against the wall
/// clock now.
///
/// An absent header, or one that is not visible ASCII, states nothing.
pub(super) fn stated_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let value = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    client_retry_after_delay(value, SystemTime::now())
}
