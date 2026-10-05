//! The operator's account of one crossed service deadline.

use super::boundary::DeadlineBoundary;
use crate::RuntimeError;
use std::error::Error;
use std::fmt;

/// The operator's account of one crossed service deadline.
///
/// It names the closed boundary the policy configured and the value configured
/// for it, so an operator reads which bound to widen rather than that
/// "something timed out". Its source is the typed
/// [`RuntimeError::DeadlineExceeded`] naming the same boundary, so a caller
/// walking the chain reaches the vocabulary rather than the sentence.
#[derive(Debug)]
pub(super) struct CrossedDeadline {
    cause: RuntimeError,
    configured: std::time::Duration,
}

impl CrossedDeadline {
    pub(super) fn new(boundary: DeadlineBoundary, configured: std::time::Duration) -> Self {
        Self {
            cause: RuntimeError::DeadlineExceeded(boundary),
            configured,
        }
    }
}

impl fmt::Display for CrossedDeadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} after {:?}", self.cause, self.configured)
    }
}

impl Error for CrossedDeadline {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}
