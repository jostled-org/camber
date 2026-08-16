//! Validation and narrowing for the scalars every service policy is built from.
//!
//! One owner for two rules the whole policy vocabulary depends on. A finite
//! duration or byte maximum is validated once, here, before any policy value
//! can hold it — zero is never a stand-in for "unbounded", which every type
//! spells with its own named constructor. And an inner layer narrows an outer
//! one through [`narrow`] alone, so runtime, server, host, and router
//! precedence cannot drift apart per dimension.

use crate::RuntimeError;
use std::time::Duration;

/// Accept a duration a policy may enforce, or refuse zero.
///
/// The name reaches the caller because the argument alone does not say which
/// bound was rejected when a builder chain sets several.
pub(super) fn finite_duration(value: Duration, name: &str) -> Result<Duration, RuntimeError> {
    match value.is_zero() {
        true => Err(RuntimeError::InvalidArgument(
            format!("{name} must be greater than zero").into_boxed_str(),
        )),
        false => Ok(value),
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
