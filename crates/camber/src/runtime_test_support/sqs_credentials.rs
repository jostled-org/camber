//! Controlled SQS credential sources, loaded by the production connect path.

use crate::mq::sqs::SqsBuilder;
use aws_credential_types::Credentials;
use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::future;
use aws_sdk_sqs::config::{ProvideCredentials, SharedCredentialsProvider};
use tokio::sync::watch;

/// Installs a credential source a real SQS connect then loads.
///
/// The probe chooses what the source answers, never how connect bounds or
/// classifies it: that stays production's.
#[doc(hidden)]
pub struct SqsCredentialProbe;

impl SqsCredentialProbe {
    /// Install a controlled credential source. Connect and operation settlement
    /// still run through the production SDK and runtime.
    pub fn with_provider(
        builder: SqsBuilder,
        provider: impl ProvideCredentials + 'static,
    ) -> SqsBuilder {
        builder.credentials_provider(SharedCredentialsProvider::new(provider))
    }

    /// A source that never answers.
    pub fn unresolved(builder: SqsBuilder) -> SqsBuilder {
        Self::with_provider(builder, Unresolved)
    }

    /// A source with no credentials to give.
    pub fn absent(builder: SqsBuilder) -> SqsBuilder {
        Self::with_provider(builder, Absent)
    }

    /// A source that answers connect's load, then never answers again.
    ///
    /// An operation that must load credentials before it signs stalls ahead
    /// of the transport, so nothing it carries is submitted. The returned
    /// handle counts every load the source was asked for.
    pub fn answers_once(builder: SqsBuilder) -> (SqsBuilder, SqsCredentialLoads) {
        let (loads, observed) = watch::channel(0);
        let source = AnswersOnce { loads };
        (
            Self::with_provider(builder, source),
            SqsCredentialLoads { observed },
        )
    }
}

/// The loads an [`SqsCredentialProbe::answers_once`] source was asked for.
#[doc(hidden)]
pub struct SqsCredentialLoads {
    observed: watch::Receiver<usize>,
}

impl SqsCredentialLoads {
    /// The loads the source was asked for so far.
    #[must_use]
    pub fn asked(&self) -> usize {
        *self.observed.borrow()
    }

    /// Resolve once the source was asked for at least `count` loads.
    pub async fn reached(&mut self, count: usize) {
        // A dropped source asks for nothing more; the wait then ends with
        // the count it reached.
        drop(self.observed.wait_for(|asked| *asked >= count).await);
    }
}

#[derive(Debug)]
struct Unresolved;

impl ProvideCredentials for Unresolved {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::new(std::future::pending())
    }
}

#[derive(Debug)]
struct Absent;

impl ProvideCredentials for Absent {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        future::ProvideCredentials::ready(Err(CredentialsError::not_loaded_no_source()))
    }
}

#[derive(Debug)]
struct AnswersOnce {
    /// The loads asked for so far: the one count, published as it changes.
    loads: watch::Sender<usize>,
}

impl ProvideCredentials for AnswersOnce {
    fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        let mut asked = 0;
        self.loads.send_modify(|loads| {
            *loads += 1;
            asked = *loads;
        });
        match asked {
            1 => future::ProvideCredentials::ready(Ok(Credentials::new(
                "camber-probe",
                "camber-probe-secret",
                None,
                None,
                "camber-probe",
            ))),
            _ => future::ProvideCredentials::new(std::future::pending()),
        }
    }
}
