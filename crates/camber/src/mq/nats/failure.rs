//! Classify SDK failures by kind, never by text.
//!
//! Each classification states whether anything reached the SDK's connection.
//! A refusal before submission is `Safe`; a lost result after submission is
//! `OutcomeUnknown`, because the server may have processed it.

use super::ack::TokensExhausted;
use crate::error::{Effect, interrupted};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
};
use async_nats::client::{
    FlushError, PublishError, PublishErrorKind, SubscribeError, SubscribeErrorKind,
};
use async_nats::jetstream::{self, ErrorCode};
use async_nats::{ConnectError, ConnectErrorKind};
use std::error::Error;
use std::sync::Arc;

/// The JetStream response code of a permission refusal.
const FORBIDDEN: usize = 403;

/// A NATS failure of `operation`.
pub(super) const fn failure(
    operation: IntegrationOperation,
    failure: IntegrationFailure,
    retryability: Retryability,
) -> IntegrationError {
    IntegrationError::new(IntegrationKind::Nats, operation, failure, retryability)
}

/// The connect handshake failed. Connecting writes nothing, so a transport
/// failure is safe to repeat.
pub(super) fn connect_failed(error: ConnectError) -> IntegrationError {
    let (kind, retryability) = match error.kind() {
        ConnectErrorKind::ServerParse => (IntegrationFailure::InvalidConfig, Retryability::Never),
        ConnectErrorKind::Authentication | ConnectErrorKind::AuthorizationViolation => {
            (IntegrationFailure::PermissionDenied, Retryability::Never)
        }
        ConnectErrorKind::TimedOut => (IntegrationFailure::Timeout, Retryability::Safe),
        ConnectErrorKind::Dns
        | ConnectErrorKind::Tls
        | ConnectErrorKind::Io
        | ConnectErrorKind::MaxReconnects => (IntegrationFailure::Unavailable, Retryability::Safe),
    };
    failure(IntegrationOperation::Connect, kind, retryability).with_source(Arc::new(error))
}

/// The readiness flush after the handshake failed. Nothing but the
/// handshake was written.
pub(super) fn readiness_lost(error: FlushError) -> IntegrationError {
    handshake_only(error)
}

/// The SDK refused the private reply subscription during connect. Nothing
/// but the handshake was written.
pub(super) fn reply_subscription_refused(error: SubscribeError) -> IntegrationError {
    handshake_only(error)
}

/// A connect that failed after writing only its handshake: safe to repeat.
fn handshake_only(error: impl Error + Send + Sync + 'static) -> IntegrationError {
    failure(
        IntegrationOperation::Connect,
        IntegrationFailure::Unavailable,
        Retryability::Safe,
    )
    .with_source(Arc::new(error))
}

/// The connection spent every reply token before this publish registered.
pub(super) const fn tokens_exhausted(exhausted: TokensExhausted) -> IntegrationError {
    let (kind, retryability) = exhausted.classification();
    failure(IntegrationOperation::Publish, kind, retryability)
}

/// No stream took the message, and a repeat applies it at most once: the
/// reply receiver stopped before the publish registered, or a correlated
/// no-responders status answered it.
pub(super) const fn no_stream_reached() -> IntegrationError {
    failure(
        IntegrationOperation::Publish,
        IntegrationFailure::Unavailable,
        Retryability::Safe,
    )
}

/// A submitted publish whose receipt is lost or unreadable: the server may
/// have stored the message.
pub(super) const fn acknowledgement_inconclusive() -> IntegrationError {
    failure(
        IntegrationOperation::Publish,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
}

/// A decoded JetStream error answered the publish, classified by its typed
/// codes. None of them makes a repeat safe.
pub(super) fn acknowledgement_refused(error: jetstream::Error) -> IntegrationError {
    let kind = match (error.code(), error.error_code()) {
        (
            _,
            ErrorCode::STREAM_MESSAGE_EXCEEDS_MAXIMUM | ErrorCode::STREAM_HEADER_EXCEEDS_MAXIMUM,
        ) => IntegrationFailure::LimitExceeded,
        (_, ErrorCode::STREAM_NOT_MATCH) => IntegrationFailure::Rejected,
        (FORBIDDEN, _) => IntegrationFailure::PermissionDenied,
        _ => IntegrationFailure::Rejected,
    };
    failure(IntegrationOperation::Publish, kind, Retryability::Never).with_source(Arc::new(error))
}

/// The SDK refused a publish before queuing it.
pub(super) fn publish_refused(error: PublishError) -> IntegrationError {
    let (kind, retryability) = match error.kind() {
        PublishErrorKind::MaxPayloadExceeded => {
            (IntegrationFailure::LimitExceeded, Retryability::Never)
        }
        PublishErrorKind::InvalidSubject => (IntegrationFailure::Rejected, Retryability::Never),
        PublishErrorKind::Send => (IntegrationFailure::Unavailable, Retryability::Safe),
    };
    failure(IntegrationOperation::Publish, kind, retryability).with_source(Arc::new(error))
}

/// The SDK refused a subscription before queuing it.
pub(super) fn subscribe_refused(error: SubscribeError) -> IntegrationError {
    let (kind, retryability) = match error.kind() {
        SubscribeErrorKind::InvalidSubject | SubscribeErrorKind::InvalidQueueName => {
            (IntegrationFailure::Rejected, Retryability::Never)
        }
        SubscribeErrorKind::Other => (IntegrationFailure::Unavailable, Retryability::Safe),
    };
    failure(IntegrationOperation::Subscribe, kind, retryability).with_source(Arc::new(error))
}

/// A submitted command's flush never completed: the server may have it.
pub(super) fn flush_lost(operation: IntegrationOperation, error: FlushError) -> IntegrationError {
    connection_replaced(operation).with_source(Arc::new(error))
}

/// The SDK replaced its connection after a command was submitted. Its
/// unwritten commands went with the old connection, so the server may or may
/// not have this one.
pub(super) const fn connection_replaced(operation: IntegrationOperation) -> IntegrationError {
    failure(
        operation,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
}

/// A close the shutdown bound cut short: an incomplete close, which
/// repeating cannot complete.
pub(super) const fn close_timed_out() -> IntegrationError {
    IntegrationError::incomplete_close(IntegrationKind::Nats)
}

/// The operation deadline passed. Before submission nothing was sent; after
/// it, the server may have the command.
pub(super) const fn deadline_passed(
    operation: IntegrationOperation,
    submitted: bool,
) -> IntegrationError {
    failure(
        operation,
        IntegrationFailure::Timeout,
        interrupted(Effect::SideEffect, submitted),
    )
}
