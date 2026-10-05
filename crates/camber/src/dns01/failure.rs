//! The typed DNS-01 failures every part of an order builds its errors from.

use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
};

/// A DNS-01 failure of `operation`.
pub(super) const fn failure(
    operation: IntegrationOperation,
    failure: IntegrationFailure,
    retryability: Retryability,
) -> IntegrationError {
    IntegrationError::new(IntegrationKind::Dns01, operation, failure, retryability)
}

/// A configuration value `operation` cannot admit.
pub(super) const fn invalid_config(operation: IntegrationOperation) -> IntegrationError {
    failure(
        operation,
        IntegrationFailure::InvalidConfig,
        Retryability::Never,
    )
}

/// `operation` cut short by a stop, carrying `retryability`.
pub(super) const fn cancelled(
    operation: IntegrationOperation,
    retryability: Retryability,
) -> IntegrationError {
    failure(operation, IntegrationFailure::Cancelled, retryability)
}

/// `operation` ended with no answer that says whether its write took effect.
pub(super) const fn outcome_unknown(operation: IntegrationOperation) -> IntegrationError {
    failure(
        operation,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
}

/// A refusal of `operation` that repeating cannot fix.
pub(super) const fn rejected(operation: IntegrationOperation) -> IntegrationError {
    failure(operation, IntegrationFailure::Rejected, Retryability::Never)
}
