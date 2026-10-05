//! One Cloudflare request and the typed reading of its answer.
//!
//! An answer is typed by HTTP status and Cloudflare error code, never by
//! message text. Its body is collected under the shared body limit before JSON
//! parsing. A redirect is refused, never followed. A lookup is read-only, so a
//! lost answer is safe to repeat, and an unreadable one is a refusal that
//! repeating cannot change. A lost or unreadable answer to a write leaves its
//! outcome unknown.

use std::fmt;
use std::sync::Arc;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::failure::{failure, outcome_unknown, rejected};
use super::transport::{BodyFailure, read_bounded};
use crate::error::{Effect, interrupted};
use crate::{IntegrationError, IntegrationFailure, IntegrationOperation, Retryability};

/// Cloudflare error codes that name a token or permission failure.
const PERMISSION_CODES: [u32; 5] = [9103, 9106, 9109, 10000, 10001];

/// The failure of an answer that was lost or could not be read.
const fn unreadable(operation: IntegrationOperation, effect: Effect) -> IntegrationError {
    match effect {
        Effect::ReadOnly => rejected(operation),
        Effect::SideEffect => outcome_unknown(operation),
    }
}

/// Send `request` for `operation` and read the result its answer carries.
///
/// `Ok(None)` is a successful answer with no result.
///
/// # Errors
///
/// The typed reading of a transport failure, a redirect, an oversized or
/// malformed body, or a refusal Cloudflare answered.
pub(super) async fn answer<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
    operation: IntegrationOperation,
    effect: Effect,
) -> Result<Option<T>, IntegrationError> {
    let response = request
        .send()
        .await
        .map_err(|error| transport(operation, effect, error))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(refused(operation, effect, status.as_u16(), None));
    }
    let body = read_bounded(response)
        .await
        .map_err(|body| body_failure(operation, effect, body))?;
    let envelope: Envelope<T> = serde_json::from_slice(&body).map_err(|error| {
        unreadable(operation, effect).with_source(Arc::new(Answered::malformed(status, error)))
    })?;
    match (status.is_success(), envelope.success) {
        (true, true) => Ok(envelope.result),
        _ => Err(refused(
            operation,
            effect,
            status.as_u16(),
            envelope.errors.first().and_then(|error| error.code),
        )),
    }
}

/// The result a successful answer must carry.
///
/// # Errors
///
/// An answer with no result reads as unreadable.
pub(super) fn required<T>(
    result: Option<T>,
    operation: IntegrationOperation,
    effect: Effect,
) -> Result<T, IntegrationError> {
    result.ok_or_else(|| unreadable(operation, effect))
}

/// Type a request that failed before any answer.
fn transport(
    operation: IntegrationOperation,
    effect: Effect,
    error: reqwest::Error,
) -> IntegrationError {
    let submitted = !error.is_connect() && !error.is_builder();
    let (failure_kind, retryability) = match (error.is_timeout(), interrupted(effect, submitted)) {
        (true, retryability) => (IntegrationFailure::Timeout, retryability),
        (false, Retryability::OutcomeUnknown) => (
            IntegrationFailure::OutcomeUnknown,
            Retryability::OutcomeUnknown,
        ),
        (false, retryability) => (IntegrationFailure::Unavailable, retryability),
    };
    failure(operation, failure_kind, retryability).with_source(Arc::new(error))
}

/// Type a body that could not be collected.
fn body_failure(
    operation: IntegrationOperation,
    effect: Effect,
    body: BodyFailure,
) -> IntegrationError {
    let typed = match (&body, effect) {
        (BodyFailure::TooLarge, Effect::ReadOnly) => failure(
            operation,
            IntegrationFailure::LimitExceeded,
            Retryability::Never,
        ),
        (BodyFailure::TooLarge, Effect::SideEffect) => failure(
            operation,
            IntegrationFailure::LimitExceeded,
            Retryability::OutcomeUnknown,
        ),
        (BodyFailure::Transport(_), Effect::ReadOnly) => failure(
            operation,
            IntegrationFailure::Unavailable,
            Retryability::Safe,
        ),
        (BodyFailure::Transport(_), Effect::SideEffect) => unreadable(operation, effect),
    };
    typed.with_source(Arc::new(body))
}

/// Type a refusal by its status and Cloudflare error code.
fn refused(
    operation: IntegrationOperation,
    effect: Effect,
    status: u16,
    code: Option<u32>,
) -> IntegrationError {
    let permission =
        matches!(status, 401 | 403) || code.is_some_and(|code| PERMISSION_CODES.contains(&code));
    let typed = match (permission, status, effect) {
        (true, _, _) => failure(
            operation,
            IntegrationFailure::PermissionDenied,
            Retryability::Never,
        ),
        (false, 429, _) | (false, 500..=599, Effect::ReadOnly) => failure(
            operation,
            IntegrationFailure::Unavailable,
            Retryability::Safe,
        ),
        (false, 500..=599, Effect::SideEffect) => unreadable(operation, effect),
        (false, _, _) => rejected(operation),
    };
    typed.with_source(Arc::new(Answered {
        status,
        code,
        parse: None,
    }))
}

/// What Cloudflare answered, kept as a refusal's inspectable source: its
/// status and error code, never its message text.
#[derive(Debug)]
struct Answered {
    status: u16,
    code: Option<u32>,
    parse: Option<serde_json::Error>,
}

impl Answered {
    /// An answer whose body did not parse, so it carries no error code.
    fn malformed(status: reqwest::StatusCode, parse: serde_json::Error) -> Self {
        Self {
            status: status.as_u16(),
            code: None,
            parse: Some(parse),
        }
    }
}

impl fmt::Display for Answered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cloudflare answered HTTP {}", self.status)?;
        match (self.code, &self.parse) {
            (Some(code), _) => write!(f, " with error code {code}"),
            (None, Some(_)) => f.write_str(" with a malformed body"),
            (None, None) => Ok(()),
        }
    }
}

impl std::error::Error for Answered {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.parse
            .as_ref()
            .map(|parse| parse as &(dyn std::error::Error + 'static))
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<CfError>,
}

#[derive(Deserialize)]
struct CfError {
    code: Option<u32>,
}
