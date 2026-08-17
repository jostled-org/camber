//! Validation and narrowing for the scalars every service policy is built from.
//!
//! One owner for two rules the whole policy vocabulary depends on. A finite
//! duration or byte maximum is validated once, here, before any policy value
//! can hold it — zero is never a stand-in for "unbounded", which every type
//! spells with its own named constructor, and neither is a bound the owner
//! enforcing it could not carry. And an inner layer narrows an outer one
//! through [`narrow`] alone, so runtime, server, host, and router precedence
//! cannot drift apart per dimension.

use crate::RuntimeError;
use std::time::Duration;

/// The longest deadline any policy dimension may carry.
///
/// Every finite deadline becomes `Instant::now() + value` at the owner that
/// enforces it, and that addition panics rather than saturating once the sum
/// leaves the clock's range. Tokio's timer picks the same thirty years and
/// calls it the far future, for the reason its own source records: a century
/// overflows on FreeBSD and a millennium on macOS. A deadline at this ceiling
/// outlives any process that could reach it, and the timers underneath Camber
/// saturate there anyway, so nothing longer can behave differently.
const MAX_POLICY_DEADLINE: Duration = Duration::from_secs(86_400 * 365 * 30);

/// Accept a duration a policy may enforce, or refuse zero and the unreachable.
///
/// The name reaches the caller because the argument alone does not say which
/// bound was rejected when a builder chain sets several. Both refusals arrive
/// here, before the value reaches a clock that would panic on it.
pub(super) fn finite_duration(value: Duration, name: &str) -> Result<Duration, RuntimeError> {
    match value {
        deadline if deadline.is_zero() => Err(RuntimeError::InvalidArgument(
            format!("{name} must be greater than zero").into_boxed_str(),
        )),
        deadline if deadline > MAX_POLICY_DEADLINE => Err(RuntimeError::InvalidArgument(
            format!(
                "{name} must be at most {} seconds",
                MAX_POLICY_DEADLINE.as_secs()
            )
            .into_boxed_str(),
        )),
        deadline => Ok(deadline),
    }
}

/// A duration brought inside the range a policy owner can enforce.
///
/// The refusing [`finite_duration`] is what a caller stating a bound gets. This
/// is for the builder setters whose long-standing signature returns `Self` and
/// so cannot report a refusal: raising zero and lowering the unreachable keeps
/// them from holding a value the enforcing clock would panic on, and warns
/// rather than adopting the caller's number in silence. Neither adjustment can
/// produce an unbounded dimension: both ends of the range are finite.
pub(super) fn clamped_duration(value: Duration, min: Duration, name: &str) -> Duration {
    match crate::time::clamp_duration(value, min, name) {
        deadline if deadline > MAX_POLICY_DEADLINE => {
            tracing::warn!(
                requested = ?deadline,
                clamped = ?MAX_POLICY_DEADLINE,
                "{name} above maximum"
            );
            MAX_POLICY_DEADLINE
        }
        deadline => deadline,
    }
}

/// Accept a whole-unit maximum a policy may enforce, or refuse zero.
///
/// Bytes and connections are both counted this way: a maximum of zero admits
/// nothing, which is never what a caller configuring a limit meant.
pub(super) fn positive_limit(value: usize, name: &str) -> Result<usize, RuntimeError> {
    match value {
        0 => Err(RuntimeError::InvalidArgument(
            format!("{name} must be at least 1").into_boxed_str(),
        )),
        limit => Ok(limit),
    }
}

/// Accept a whole-unit maximum the owner enforcing it can hold.
///
/// Zero admits nothing, and a value above the enforcing primitive's own
/// ceiling is a bound no owner can carry. Both refusals name the dimension and
/// the ceiling here, before the value reaches an owner that would panic on it.
pub(super) fn limit_within(
    value: usize,
    ceiling: usize,
    name: &str,
) -> Result<usize, RuntimeError> {
    match positive_limit(value, name)? {
        limit if limit > ceiling => Err(RuntimeError::InvalidArgument(
            format!("{name} must be at most {ceiling}").into_boxed_str(),
        )),
        limit => Ok(limit),
    }
}

/// The bound that survives when an inner layer is applied under an outer one.
///
/// `None` is unbounded on either side. An inner unbounded value therefore
/// inherits the outer bound rather than erasing it, and two finite values
/// resolve to the smaller — the earliest deadline, the smallest maximum.
pub(super) fn narrow<T: Ord>(inner: Option<T>, outer: Option<T>) -> Option<T> {
    match (inner, outer) {
        (Some(inner), Some(outer)) => Some(inner.min(outer)),
        (Some(only), None) | (None, Some(only)) => Some(only),
        (None, None) => None,
    }
}
