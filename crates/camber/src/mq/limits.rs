//! The bounds every managed message-queue instance shares.
//!
//! One definition of the defaults and of what a valid bound is, so each
//! adapter's builder validates the same way before it constructs an SDK
//! client. There is no zero-as-unbounded spelling: every duration is positive
//! and at most a day, and every count is positive and fits the queue
//! primitives that enforce it.

use crate::error::valid_duration;
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// The largest count any bound accepts: the most permits a Tokio semaphore or
/// channel can hold.
const MAX_COUNT: usize = tokio::sync::Semaphore::MAX_PERMITS;

/// The default connect bound.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The default bound on one operation, SDK queuing included.
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

/// The default bound on a local close.
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The default number of operations one instance runs at once.
pub(crate) const DEFAULT_COUNT: usize = 64;

/// The default largest message payload, in bytes.
const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// The bounds one message-queue instance runs under.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueueLimits {
    pub(crate) connect_timeout: Duration,
    pub(crate) operation_timeout: Duration,
    pub(crate) shutdown_timeout: Duration,
    pub(crate) max_in_flight: usize,
    pub(crate) max_message_bytes: usize,
}

impl QueueLimits {
    /// The documented defaults.
    pub(crate) const DEFAULT: Self = Self {
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        operation_timeout: DEFAULT_OPERATION_TIMEOUT,
        shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
        max_in_flight: DEFAULT_COUNT,
        max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
    };

    /// Check every bound.
    ///
    /// # Errors
    ///
    /// `InvalidConfig` naming the first bound out of range.
    pub(crate) fn validate(&self, kind: IntegrationKind) -> Result<(), IntegrationError> {
        validate_duration(kind, "connect_timeout", self.connect_timeout)?;
        validate_duration(kind, "operation_timeout", self.operation_timeout)?;
        validate_duration(kind, "shutdown_timeout", self.shutdown_timeout)?;
        validate_count(kind, "max_in_flight", self.max_in_flight)?;
        validate_count(kind, "max_message_bytes", self.max_message_bytes)
    }
}

/// Accept a positive duration of at most a day.
///
/// # Errors
///
/// `InvalidConfig` naming `bound`.
pub(crate) fn validate_duration(
    kind: IntegrationKind,
    bound: &'static str,
    value: Duration,
) -> Result<(), IntegrationError> {
    check_setting(kind, bound, valid_duration(value))
}

/// Accept a positive count the enforcing primitives can hold.
///
/// # Errors
///
/// `InvalidConfig` naming `bound`.
pub(crate) fn validate_count(
    kind: IntegrationKind,
    bound: &'static str,
    value: usize,
) -> Result<(), IntegrationError> {
    check_setting(kind, bound, (1..=MAX_COUNT).contains(&value))
}

/// Accept a setting only when `valid`.
///
/// # Errors
///
/// `InvalidConfig` naming `setting`, before any effect.
pub(crate) fn check_setting(
    kind: IntegrationKind,
    setting: &'static str,
    valid: bool,
) -> Result<(), IntegrationError> {
    match valid {
        true => Ok(()),
        false => Err(invalid_setting(kind, setting)),
    }
}

/// The refusal an invalid setting answers with. It names the setting, never
/// its value.
pub(crate) fn invalid_setting(kind: IntegrationKind, setting: &'static str) -> IntegrationError {
    invalid_config(kind).with_source(Arc::new(InvalidSetting { setting }))
}

/// Refuse a message body over `max_message_bytes` before copying it on send,
/// or before delivering it on receive.
///
/// # Errors
///
/// `LimitExceeded`, never retryable: the same body is refused again.
pub(crate) const fn admit_message(
    kind: IntegrationKind,
    operation: IntegrationOperation,
    bytes: usize,
    max_message_bytes: usize,
) -> Result<(), IntegrationError> {
    match bytes <= max_message_bytes {
        true => Ok(()),
        false => Err(IntegrationError::new(
            kind,
            operation,
            IntegrationFailure::LimitExceeded,
            Retryability::Never,
        )),
    }
}

/// A configuration refusal: nothing was constructed or sent.
pub(crate) const fn invalid_config(kind: IntegrationKind) -> IntegrationError {
    IntegrationError::new(
        kind,
        IntegrationOperation::Connect,
        IntegrationFailure::InvalidConfig,
        Retryability::Never,
    )
}

/// The setting a builder rejected, kept as the refusal's inspectable source.
#[derive(Debug)]
struct InvalidSetting {
    setting: &'static str,
}

impl fmt::Display for InvalidSetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is invalid", self.setting)
    }
}

impl std::error::Error for InvalidSetting {}
