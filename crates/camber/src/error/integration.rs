//! One integration operation's failure, as the caller and the runtime hold it.
//!
//! The failure is described in a closed vocabulary. Every enum here is closed
//! and exhaustively matchable, and each value has one bounded label. The labels
//! are the only form these values take in operator events and metric labels, so
//! a label set cannot grow with user input.

use super::RuntimeError;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// The managed integration a failure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntegrationKind {
    /// A Core NATS connection.
    Nats,
    /// An Amazon SQS queue client.
    Sqs,
    /// An ACME DNS-01 certificate owner.
    Dns01,
}

impl IntegrationKind {
    /// The bounded name this kind is reported under.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Nats => "nats",
            Self::Sqs => "sqs",
            Self::Dns01 => "dns01",
        }
    }
}

/// The integration operation that failed.
///
/// `Publish` covers both a NATS publish and an SQS send; `Delete` is an SQS
/// message deletion. A DNS record deletion is `DeleteTxt`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntegrationOperation {
    /// Establishing the integration's connection or client.
    Connect,
    /// Confirming the integration can serve operations.
    Ready,
    /// Sending one message.
    Publish,
    /// Registering one subscription.
    Subscribe,
    /// Taking delivered messages.
    Receive,
    /// Deleting one received message.
    Delete,
    /// Closing an integration or one of its subscriptions.
    Close,
    /// Finding the DNS zone that owns a domain.
    ZoneLookup,
    /// Creating one challenge TXT record.
    CreateTxt,
    /// Deleting one challenge TXT record.
    DeleteTxt,
    /// Issuing one certificate.
    Provision,
    /// Reading a cached certificate generation.
    CacheRead,
    /// Publishing a certificate generation to the cache.
    CacheWrite,
    /// Renewing a certificate.
    Renew,
}

impl IntegrationOperation {
    /// The bounded name this operation is reported under.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Ready => "ready",
            Self::Publish => "publish",
            Self::Subscribe => "subscribe",
            Self::Receive => "receive",
            Self::Delete => "delete",
            Self::Close => "close",
            Self::ZoneLookup => "zone_lookup",
            Self::CreateTxt => "create_txt",
            Self::DeleteTxt => "delete_txt",
            Self::Provision => "provision",
            Self::CacheRead => "cache_read",
            Self::CacheWrite => "cache_write",
            Self::Renew => "renew",
        }
    }
}

/// Why an integration operation failed.
///
/// Adapters choose the value from SDK error kinds and protocol status codes,
/// never from diagnostic text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntegrationFailure {
    /// The configuration was refused before any effect.
    InvalidConfig,
    /// The service could not be reached or is not ready.
    Unavailable,
    /// The service refused the credentials or the permission.
    PermissionDenied,
    /// The service explicitly refused the operation.
    Rejected,
    /// A Camber admission bound was full; nothing was submitted.
    Busy,
    /// A declared size or count bound was crossed.
    LimitExceeded,
    /// The operation's deadline expired.
    Timeout,
    /// The operation was cancelled.
    Cancelled,
    /// The integration or its runtime had closed.
    Closed,
    /// A submitted write lost its outcome; it may or may not have taken effect.
    OutcomeUnknown,
    /// Cleanup left records the caller must resolve.
    CleanupIncomplete,
    /// A certificate failed identity or validity checks.
    InvalidCertificate,
}

impl IntegrationFailure {
    /// The bounded name this failure is reported under.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::InvalidConfig => "invalid_config",
            Self::Unavailable => "unavailable",
            Self::PermissionDenied => "permission_denied",
            Self::Rejected => "rejected",
            Self::Busy => "busy",
            Self::LimitExceeded => "limit_exceeded",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::Closed => "closed",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::CleanupIncomplete => "cleanup_incomplete",
            Self::InvalidCertificate => "invalid_certificate",
        }
    }
}

/// Whether repeating a failed operation is safe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Retryability {
    /// Repeating the operation cannot succeed without a change.
    Never,
    /// Repeating the operation cannot apply it twice: nothing was submitted,
    /// the service refused it before it took effect, or the operation only
    /// reads.
    Safe,
    /// A write was submitted and its outcome was lost; repeating it may apply
    /// it twice.
    OutcomeUnknown,
}

impl Retryability {
    /// The bounded name this advice is reported under.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Safe => "safe",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// Whether an operation can change the service's state.
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
#[derive(Clone, Copy)]
pub(crate) enum Effect {
    /// A query: repeating it is always safe. Core NATS has none.
    #[cfg(any(feature = "sqs", feature = "dns01"))]
    ReadOnly,
    /// A send, receive, delete, or record write.
    SideEffect,
}

/// The retryability of an interrupted operation: unknown only when a
/// side-effecting request was submitted.
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
pub(crate) const fn interrupted(effect: Effect, submitted: bool) -> Retryability {
    match (effect, submitted) {
        (Effect::SideEffect, true) => Retryability::OutcomeUnknown,
        (Effect::SideEffect, false) => Retryability::Safe,
        #[cfg(any(feature = "sqs", feature = "dns01"))]
        (Effect::ReadOnly, _) => Retryability::Safe,
    }
}

/// The longest duration any integration bound accepts.
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
const MAX_DURATION: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Whether `value` is a valid integration bound: positive and at most a day.
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
pub(crate) fn valid_duration(value: std::time::Duration) -> bool {
    !value.is_zero() && value <= MAX_DURATION
}

/// Each closed value displays as its bounded label and nothing else.
macro_rules! display_label {
    ($($closed:ty),+) => {$(
        impl fmt::Display for $closed {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.label())
            }
        }
    )+};
}

display_label!(
    IntegrationKind,
    IntegrationOperation,
    IntegrationFailure,
    Retryability
);

/// The third-party failure an integration error keeps for inspection.
///
/// Shared, because the caller, the runtime settlement, and the operator event
/// can all hold the same error.
type IntegrationSource = Arc<dyn Error + Send + Sync>;

/// The most unresolved records one DNS order's account keeps.
pub(crate) const MAX_CLEANUP_ITEMS: usize = 100;

/// One unresolved record a DNS cleanup left behind.
///
/// Carries no challenge secret and no TXT value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupItem {
    domain: Box<str>,
    record_id: Option<Box<str>>,
    failure: IntegrationFailure,
}

impl CleanupItem {
    /// Record one unresolved cleanup.
    pub(crate) const fn new(
        domain: Box<str>,
        record_id: Option<Box<str>>,
        failure: IntegrationFailure,
    ) -> Self {
        Self {
            domain,
            record_id,
            failure,
        }
    }

    /// The domain whose challenge record is unresolved.
    #[must_use]
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// The provider's record ID, when the provider acknowledged the create.
    ///
    /// `None` means the create was submitted and its outcome was lost, so the
    /// record may exist without a known ID.
    #[must_use]
    pub fn record_id(&self) -> Option<&str> {
        self.record_id.as_deref()
    }

    /// Why this record is unresolved.
    #[must_use]
    pub const fn failure(&self) -> IntegrationFailure {
        self.failure
    }
}

/// One managed integration operation's failure.
///
/// The closed fields say what failed and whether repeating it is safe. The
/// third-party source stays inspectable through [`Error::source`], and neither
/// `Display` nor `Debug` renders it: an SDK error can echo the token,
/// credentials, payload, receipt handle, or URL of the request it failed.
#[derive(Clone)]
pub struct IntegrationError {
    kind: IntegrationKind,
    operation: IntegrationOperation,
    failure: IntegrationFailure,
    retryability: Retryability,
    instance_id: Option<u64>,
    cleanup: Option<Arc<[CleanupItem]>>,
    source: Option<IntegrationSource>,
}

impl IntegrationError {
    /// The factory every integration failure is built through.
    pub(crate) const fn new(
        kind: IntegrationKind,
        operation: IntegrationOperation,
        failure: IntegrationFailure,
        retryability: Retryability,
    ) -> Self {
        Self {
            kind,
            operation,
            failure,
            retryability,
            instance_id: None,
            cleanup: None,
            source: None,
        }
    }

    /// A close its bound cut short: incomplete, and repeating it cannot
    /// complete it.
    pub(crate) const fn incomplete_close(kind: IntegrationKind) -> Self {
        Self::new(
            kind,
            IntegrationOperation::Close,
            IntegrationFailure::Timeout,
            Retryability::Never,
        )
    }

    /// Name the admitted instance the failure belongs to.
    pub(crate) const fn with_instance(mut self, id: u64) -> Self {
        self.instance_id = Some(id);
        self
    }

    /// Keep the third-party failure, shared rather than copied.
    pub(crate) fn with_source(mut self, source: IntegrationSource) -> Self {
        self.source = Some(source);
        self
    }

    /// Attach the records a cleanup left unresolved, at most
    /// [`MAX_CLEANUP_ITEMS`] of them.
    ///
    /// One order creates at most one record per domain, and an order admits at
    /// most that many domains, so the bound keeps every record an order can
    /// leave. It holds the account's size even for a caller that breaks it.
    pub(crate) fn with_cleanup(mut self, mut items: Vec<CleanupItem>) -> Self {
        items.truncate(MAX_CLEANUP_ITEMS);
        self.cleanup = Some(items.into());
        self
    }

    /// The integration the failure came from.
    #[must_use]
    pub const fn kind(&self) -> IntegrationKind {
        self.kind
    }

    /// The operation that failed.
    #[must_use]
    pub const fn operation(&self) -> IntegrationOperation {
        self.operation
    }

    /// Why the operation failed.
    #[must_use]
    pub const fn failure(&self) -> IntegrationFailure {
        self.failure
    }

    /// Whether repeating the operation is safe.
    #[must_use]
    pub const fn retryability(&self) -> Retryability {
        self.retryability
    }

    /// The runtime-local identity admission assigned, or `None` when the
    /// failure came before admission.
    #[must_use]
    pub const fn instance_id(&self) -> Option<u64> {
        self.instance_id
    }

    /// Every record a cleanup left unresolved; empty outside cleanup failures.
    #[must_use]
    pub fn cleanup(&self) -> &[CleanupItem] {
        self.cleanup.as_deref().unwrap_or_default()
    }
}

impl fmt::Display for IntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "integration {} {} failed: {} (retry: {})",
            self.kind, self.operation, self.failure, self.retryability
        )?;
        if let Some(id) = self.instance_id {
            write!(f, ", instance {id}")?;
        }
        match self.cleanup().len() {
            0 => Ok(()),
            unresolved => write!(f, ", {unresolved} cleanup records unresolved"),
        }
    }
}

impl fmt::Debug for IntegrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntegrationError")
            .field("kind", &self.kind)
            .field("operation", &self.operation)
            .field("failure", &self.failure)
            .field("retryability", &self.retryability)
            .field("instance_id", &self.instance_id)
            .field("cleanup", &self.cleanup())
            .field("has_source", &self.source.is_some())
            .finish()
    }
}

impl Error for IntegrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

/// Whether a diagnostic chain must stop after rendering `error`.
///
/// An integration error's sources are third-party text that can carry secrets,
/// so a chain walker renders the integration error itself and nothing below it.
/// Matches the error however it was carried: bare, shared, or as the runtime
/// arm, whose transparent `source` skips straight to the third-party failure.
pub(crate) fn hides_sources(error: &(dyn Error + 'static)) -> bool {
    error.is::<IntegrationError>()
        || error.is::<Arc<IntegrationError>>()
        || matches!(
            error.downcast_ref::<RuntimeError>(),
            Some(RuntimeError::Integration(_))
        )
}
