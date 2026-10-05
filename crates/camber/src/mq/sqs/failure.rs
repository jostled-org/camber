//! Classify SDK failures by kind, error code, and status, never by text.
//!
//! Permission and configuration failures are `Never` retryable. Transport loss,
//! timeout, or cancellation is `Safe` before submission or for a read-only query.
//! A side-effecting request whose answer was lost after submission is
//! `OutcomeUnknown`: send, receive, and delete can each have happened, because
//! receive changes visibility.
//!
//! A service answer is typed by its error code first, because the SQS JSON
//! protocol answers throttles and some denials with status 400. Its status
//! decides only when Camber does not recognize the code.

use crate::error::{Effect, interrupted};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
};
use aws_credential_types::provider::error::CredentialsError;
use aws_sdk_sqs::config::http::HttpResponse;
use aws_sdk_sqs::error::{ConnectorError, ProvideErrorMetadata, SdkError};
use std::error::Error;
use std::sync::Arc;

/// An SQS failure of `operation`.
pub(super) const fn failure(
    operation: IntegrationOperation,
    failure: IntegrationFailure,
    retryability: Retryability,
) -> IntegrationError {
    IntegrationError::new(IntegrationKind::Sqs, operation, failure, retryability)
}

/// A request whose answer was lost: unknown when a side effect may have
/// happened, otherwise unavailable and safe.
const fn lost(
    operation: IntegrationOperation,
    effect: Effect,
    submitted: bool,
) -> IntegrationError {
    match interrupted(effect, submitted) {
        Retryability::OutcomeUnknown => failure(
            operation,
            IntegrationFailure::OutcomeUnknown,
            Retryability::OutcomeUnknown,
        ),
        retryability => failure(operation, IntegrationFailure::Unavailable, retryability),
    }
}

/// Classify one SDK failure of `operation`.
pub(super) fn sdk_failed<E>(
    operation: IntegrationOperation,
    effect: Effect,
    submitted: bool,
    error: SdkError<E, HttpResponse>,
) -> IntegrationError
where
    E: Error + ProvideErrorMetadata + Send + Sync + 'static,
{
    let classified = match &error {
        SdkError::ConstructionFailure(_) => {
            failure(operation, IntegrationFailure::Rejected, Retryability::Never)
        }
        SdkError::TimeoutError(_) => failure(
            operation,
            IntegrationFailure::Timeout,
            interrupted(effect, submitted),
        ),
        SdkError::DispatchFailure(dispatch)
            if dispatch.as_connector_error().is_some_and(never_connected) =>
        {
            failure(
                operation,
                IntegrationFailure::Unavailable,
                Retryability::Safe,
            )
        }
        SdkError::ServiceError(service) => refused(
            operation,
            effect,
            service.err().code().and_then(coded),
            service.raw().status().as_u16(),
        ),
        _ => lost(operation, effect, submitted),
    };
    classified.with_source(Arc::new(error))
}

/// What a service error code names, ahead of its status.
#[derive(Clone, Copy)]
enum Coded {
    /// The service refused the request before it took effect; it may be
    /// repeated later.
    Throttled,
    /// The credentials or the permission were refused.
    Denied,
    /// The service failed while handling the request.
    ServerFailed,
}

/// The class of a service error code Camber recognizes.
fn coded(code: &str) -> Option<Coded> {
    match code {
        "RequestThrottled"
        | "ThrottlingException"
        | "Throttling"
        | "KmsThrottled"
        | "OverLimit" => Some(Coded::Throttled),
        "AccessDenied"
        | "AccessDeniedException"
        | "KmsAccessDenied"
        | "InvalidSecurity"
        | "InvalidClientTokenId"
        | "UnrecognizedClientException"
        | "SignatureDoesNotMatch"
        | "MissingAuthenticationToken"
        | "IncompleteSignature" => Some(Coded::Denied),
        "InternalError" | "InternalFailure" | "ServiceUnavailable" => Some(Coded::ServerFailed),
        _ => None,
    }
}

/// The service answered. A recognized code decides first; otherwise a 4xx
/// is a conclusive refusal, and anything else leaves a side effect unknown.
fn refused(
    operation: IntegrationOperation,
    effect: Effect,
    code: Option<Coded>,
    status: u16,
) -> IntegrationError {
    match (code, status) {
        (Some(Coded::Throttled), _) => failure(
            operation,
            IntegrationFailure::Unavailable,
            Retryability::Safe,
        ),
        (Some(Coded::Denied), _) | (None, 401 | 403) => failure(
            operation,
            IntegrationFailure::PermissionDenied,
            Retryability::Never,
        ),
        (None, 400..=499) => failure(operation, IntegrationFailure::Rejected, Retryability::Never),
        (Some(Coded::ServerFailed), _) | (None, _) => failure(
            operation,
            IntegrationFailure::Unavailable,
            interrupted(effect, true),
        ),
    }
}

/// Whether the transport failed while connecting, before any request byte
/// could be written.
fn never_connected(error: &ConnectorError) -> bool {
    let first: &(dyn Error + 'static) = error;
    std::iter::successors(Some(first), |current| (*current).source()).any(|current| {
        current
            .downcast_ref::<hyper_util::client::legacy::Error>()
            .is_some_and(hyper_util::client::legacy::Error::is_connect)
    })
}

/// Credential loading failed during connect. Nothing was sent to the queue.
pub(super) fn credentials_failed(error: CredentialsError) -> IntegrationError {
    let (kind, retryability) = match &error {
        CredentialsError::CredentialsNotLoaded(_) | CredentialsError::InvalidConfiguration(_) => {
            (IntegrationFailure::InvalidConfig, Retryability::Never)
        }
        CredentialsError::ProviderTimedOut(_) => (IntegrationFailure::Timeout, Retryability::Safe),
        _ => (IntegrationFailure::Unavailable, Retryability::Safe),
    };
    failure(IntegrationOperation::Connect, kind, retryability).with_source(Arc::new(error))
}

/// The operation deadline passed.
pub(super) const fn deadline_passed(
    operation: IntegrationOperation,
    effect: Effect,
    submitted: bool,
) -> IntegrationError {
    failure(
        operation,
        IntegrationFailure::Timeout,
        interrupted(effect, submitted),
    )
}

/// A close cut the operation short.
pub(super) const fn cut_by_close(
    operation: IntegrationOperation,
    effect: Effect,
    submitted: bool,
) -> IntegrationError {
    failure(
        operation,
        IntegrationFailure::Cancelled,
        interrupted(effect, submitted),
    )
}

/// A send answered without the message ID that names its acknowledgement:
/// the queue may hold the message.
pub(super) const fn unacknowledged_send() -> IntegrationError {
    failure(
        IntegrationOperation::Publish,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
}

/// A caller input refused before submission.
pub(super) const fn invalid_input(operation: IntegrationOperation) -> IntegrationError {
    failure(operation, IntegrationFailure::Rejected, Retryability::Never)
}

/// A batch over its bounds: the receive happened, and its messages return to
/// the queue when their visibility ends.
pub(super) const fn batch_over_limit() -> IntegrationError {
    failure(
        IntegrationOperation::Receive,
        IntegrationFailure::LimitExceeded,
        Retryability::Never,
    )
}
