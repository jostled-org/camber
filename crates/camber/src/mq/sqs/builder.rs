//! The validated configuration of one SQS client.

use super::client::{self, Client};
use crate::mq::limits::{QueueLimits, check_setting};
use crate::{IntegrationError, IntegrationKind, RuntimeError};
use aws_sdk_sqs::config::SharedCredentialsProvider;
use std::fmt;
use std::time::Duration;

/// The configuration of one SQS client.
///
/// Setters consume the builder. Nothing is checked until [`Self::connect`],
/// which validates the whole configuration before it creates an SDK client.
/// Explicit credentials belong to this client alone and never touch the
/// process environment.
#[derive(Clone, Debug)]
#[must_use]
pub struct SqsBuilder {
    limits: QueueLimits,
    region: Option<Box<str>>,
    endpoint: Option<Box<str>>,
    credentials: CredentialSource,
}

/// Where a client's credentials come from.
#[derive(Clone)]
pub(super) enum CredentialSource {
    /// The SDK's default chain, loaded under the connect deadline.
    Chain,
    /// Keys for this client alone.
    Explicit {
        access_key: Box<str>,
        secret_key: Box<str>,
        session_token: Option<Box<str>>,
    },
    /// A provider a test probe installed in place of the chain.
    Provided(SharedCredentialsProvider),
}

impl fmt::Debug for CredentialSource {
    /// Names the source only: keys and tokens are never rendered.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Chain => "Chain",
            Self::Explicit { .. } => "Explicit",
            Self::Provided(_) => "Provided",
        })
    }
}

/// A validated configuration, ready to construct an SDK client from.
pub(super) struct SqsSettings {
    pub(super) limits: QueueLimits,
    pub(super) region: Option<Box<str>>,
    pub(super) endpoint: Option<Box<str>>,
    pub(super) credentials: CredentialSource,
}

/// A builder with the default bounds, the SDK's region and credential chain,
/// and the service's own endpoint.
///
/// Pure: it needs no runtime and performs no I/O.
pub fn builder() -> SqsBuilder {
    SqsBuilder {
        limits: QueueLimits::DEFAULT,
        region: None,
        endpoint: None,
        credentials: CredentialSource::Chain,
    }
}

/// Connect a client with the default bounds.
///
/// # Errors
///
/// See [`SqsBuilder::connect`].
pub async fn connect() -> Result<Client, RuntimeError> {
    builder().connect().await
}

impl SqsBuilder {
    /// Bound each operation, from admission through the SDK's request to its
    /// response. Default 30 seconds; positive and at most 24 hours.
    pub fn operation_timeout(mut self, timeout: Duration) -> Self {
        self.limits.operation_timeout = timeout;
        self
    }

    /// Bound configuration and credential loading. Default 10 seconds;
    /// positive and at most 24 hours.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.limits.connect_timeout = timeout;
        self
    }

    /// Bound how long a close waits for running operations before it cuts
    /// them. Default 5 seconds; positive and at most 24 hours.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.limits.shutdown_timeout = timeout;
        self
    }

    /// The most operations running at once. Default 64.
    pub fn max_in_flight(mut self, max: usize) -> Self {
        self.limits.max_in_flight = max;
        self
    }

    /// The largest message body sent or delivered, in bytes. Default 1 MiB.
    pub fn max_message_bytes(mut self, max: usize) -> Self {
        self.limits.max_message_bytes = max;
        self
    }

    /// The AWS region, in place of the SDK's region chain.
    pub fn region(mut self, region: &str) -> Self {
        self.region = Some(region.into());
        self
    }

    /// An absolute `http` or `https` endpoint, in place of the service's
    /// own.
    pub fn endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Credentials for this client alone, in place of the SDK's chain.
    pub fn credentials(
        mut self,
        access_key: &str,
        secret_key: &str,
        session_token: Option<&str>,
    ) -> Self {
        self.credentials = CredentialSource::Explicit {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token: session_token.map(Box::from),
        };
        self
    }

    /// The bounds this builder hands to connect.
    pub(crate) const fn limits(&self) -> QueueLimits {
        self.limits
    }

    /// Replace the credential source with `provider`.
    pub(crate) fn credentials_provider(mut self, provider: SharedCredentialsProvider) -> Self {
        self.credentials = CredentialSource::Provided(provider);
        self
    }

    /// Validate the configuration, admit the client to the current runtime,
    /// and load its configuration and credentials.
    ///
    /// Connecting sends no request: readiness is [`Client::ready`].
    ///
    /// # Errors
    ///
    /// `InvalidConfig` for a bound out of range, an empty or malformed region,
    /// an endpoint that is not an absolute `http` or `https` URL without
    /// userinfo or query, or empty explicit credentials; then `NoRuntime`
    /// outside a Camber runtime, `ScopeClosed` once its admission closed, and
    /// `Busy` when its integrations are full. None of these perform I/O, and
    /// each is one connect terminal with no instance and no duration. After
    /// admission: `InvalidConfig` when no region or credentials load,
    /// `Unavailable` when a credential source fails, and `Timeout` when
    /// loading outlives the connect bound.
    pub async fn connect(self) -> Result<Client, RuntimeError> {
        let max_in_flight = self.limits.max_in_flight;
        crate::mq::connect::connect(
            IntegrationKind::Sqs,
            self.validate(),
            max_in_flight,
            client::establish,
        )
        .await
    }

    /// Check the whole configuration, constructing nothing.
    fn validate(self) -> Result<SqsSettings, IntegrationError> {
        let kind = IntegrationKind::Sqs;
        self.limits.validate(kind)?;
        check_setting(
            kind,
            "region",
            self.region.as_deref().is_none_or(valid_region),
        )?;
        check_setting(
            kind,
            "endpoint",
            self.endpoint.as_deref().is_none_or(valid_endpoint),
        )?;
        check_setting(kind, "credentials", valid_credentials(&self.credentials))?;
        Ok(SqsSettings {
            limits: self.limits,
            region: self.region,
            endpoint: self.endpoint,
            credentials: self.credentials,
        })
    }
}

/// A region is one nonempty run of ASCII letters, digits, and hyphens.
fn valid_region(region: &str) -> bool {
    !region.is_empty()
        && region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// An endpoint is an absolute `http` or `https` URL with a host, and neither
/// userinfo nor a query: it names where requests go, never who sends them.
fn valid_endpoint(endpoint: &str) -> bool {
    endpoint.parse::<hyper::Uri>().is_ok_and(|uri| {
        let scheme = matches!(uri.scheme_str(), Some("http" | "https"));
        let authority = uri.authority().is_some_and(|authority| {
            !authority.host().is_empty() && !authority.as_str().contains('@')
        });
        scheme && authority && uri.query().is_none()
    })
}

/// Explicit keys and a given session token must be nonempty.
fn valid_credentials(source: &CredentialSource) -> bool {
    match source {
        CredentialSource::Explicit {
            access_key,
            secret_key,
            session_token,
        } => {
            !access_key.is_empty()
                && !secret_key.is_empty()
                && session_token
                    .as_deref()
                    .is_none_or(|token| !token.is_empty())
        }
        CredentialSource::Chain | CredentialSource::Provided(_) => true,
    }
}
