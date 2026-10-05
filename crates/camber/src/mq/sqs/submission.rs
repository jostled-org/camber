//! The SDK hook that records when a request reaches the transport.

use crate::integration_lifecycle::SubmissionMark;
use aws_sdk_sqs::config::interceptors::BeforeTransmitInterceptorContextRef;
use aws_sdk_sqs::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_sdk_sqs::error::BoxError;
use std::fmt;

/// Sets one operation's submission mark just before the SDK hands its
/// request to the HTTP client.
///
/// Every attempt passes through this hook, and Camber configures one attempt,
/// so the mark names the one request that may have reached the service.
pub(super) struct MarkSubmission {
    mark: SubmissionMark,
}

impl MarkSubmission {
    pub(super) const fn new(mark: SubmissionMark) -> Self {
        Self { mark }
    }
}

impl fmt::Debug for MarkSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MarkSubmission")
            .field("submitted", &self.mark.submitted())
            .finish()
    }
}

impl Intercept for MarkSubmission {
    fn name(&self) -> &'static str {
        "CamberSubmissionMark"
    }

    fn read_before_transmit(
        &self,
        _: &BeforeTransmitInterceptorContextRef<'_>,
        _: &RuntimeComponents,
        _: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        self.mark.submit();
        Ok(())
    }
}
