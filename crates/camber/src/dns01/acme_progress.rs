//! Retry classification from the request the ACME client actually submitted.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;

use super::acme_transport::AcmeTransportFailure;
use super::failure::failure;
use crate::{IntegrationError, IntegrationFailure, IntegrationOperation, Retryability};

/// Shared only between one sequential ACME client and its order's stop path.
#[derive(Clone, Default)]
pub(super) struct AcmeProgress {
    submitted_write: Arc<AtomicBool>,
}

#[derive(Deserialize)]
struct SignedPayload<'a> {
    payload: &'a str,
}

impl AcmeProgress {
    /// Record submission, including ACME's empty-payload POST-as-GET reads.
    pub(super) fn submitting(&self, request: &reqwest::Request) {
        let is_read = matches!(
            *request.method(),
            reqwest::Method::GET | reqwest::Method::HEAD
        ) || request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .is_some_and(|body| {
                serde_json::from_slice::<SignedPayload<'_>>(body)
                    .is_ok_and(|signed| signed.payload.is_empty())
            });
        self.submitted_write.store(!is_read, Ordering::SeqCst);
    }

    /// A completed SDK operation or failed connection leaves no uncertain write.
    pub(super) fn settled(&self) {
        self.submitted_write.store(false, Ordering::SeqCst);
    }

    pub(super) fn retryability(&self) -> Retryability {
        match self.submitted_write.load(Ordering::SeqCst) {
            true => Retryability::OutcomeUnknown,
            false => Retryability::Safe,
        }
    }

    /// Keep the submission fact until the SDK has accepted the response.
    pub(super) fn finish<T>(
        &self,
        result: Result<T, instant_acme::Error>,
    ) -> Result<T, IntegrationError> {
        match result {
            Ok(value) => {
                self.settled();
                Ok(value)
            }
            Err(error) => Err(self.classify(error)),
        }
    }

    fn classify(&self, error: instant_acme::Error) -> IntegrationError {
        let (kind, retry) = match &error {
            instant_acme::Error::Api(problem) if explicit_retry(problem) => {
                (IntegrationFailure::Unavailable, Retryability::Safe)
            }
            instant_acme::Error::Api(problem) if !server_failure(problem) => {
                (IntegrationFailure::Rejected, Retryability::Never)
            }
            _ if read_failure(&error).0 == IntegrationFailure::Timeout => {
                (IntegrationFailure::Timeout, self.retryability())
            }
            _ if self.retryability() == Retryability::OutcomeUnknown => (
                IntegrationFailure::OutcomeUnknown,
                Retryability::OutcomeUnknown,
            ),
            _ => read_failure(&error),
        };
        failure(IntegrationOperation::Provision, kind, retry).with_source(Arc::new(error))
    }
}

fn has_type(problem: &instant_acme::Problem, suffix: &str) -> bool {
    problem
        .r#type
        .as_deref()
        .is_some_and(|kind| kind.ends_with(suffix))
}

fn explicit_retry(problem: &instant_acme::Problem) -> bool {
    !server_failure(problem)
        && (problem.status == Some(429)
            || has_type(problem, ":rateLimited")
            || has_type(problem, ":badNonce"))
}

fn server_failure(problem: &instant_acme::Problem) -> bool {
    matches!(problem.status, Some(500..=599)) || has_type(problem, ":serverInternal")
}

fn read_failure(error: &instant_acme::Error) -> (IntegrationFailure, Retryability) {
    let transport = match error {
        instant_acme::Error::Other(inner) => inner.downcast_ref::<AcmeTransportFailure>(),
        _ => None,
    };
    match (transport, error) {
        (Some(AcmeTransportFailure::Timeout), _) | (None, instant_acme::Error::Timeout(_)) => {
            (IntegrationFailure::Timeout, Retryability::Safe)
        }
        (Some(AcmeTransportFailure::TooLarge), _) => {
            (IntegrationFailure::LimitExceeded, Retryability::Never)
        }
        (Some(_), _) | (None, instant_acme::Error::Api(_)) => {
            (IntegrationFailure::Unavailable, Retryability::Safe)
        }
        _ => (IntegrationFailure::Rejected, Retryability::Never),
    }
}
