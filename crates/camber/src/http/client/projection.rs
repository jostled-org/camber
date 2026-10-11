//! The one projection from a transport failure to a client error category.
//!
//! Classification reads typed facts only: Reqwest's own predicates and the
//! type of each cause. No text is matched and none is kept, so the category is
//! the whole account a caller receives.

use super::super::checked_collect::CollectionFailure;
use super::error::{ClientError, ClientRedirect, ClientTimeout};
use crate::RuntimeError;
use std::error::Error;

/// The most causes one failure is read through.
///
/// A real chain is a handful of links. The bound ends the walk on a chain that
/// loops instead of trusting every third-party `source` to terminate.
const CAUSE_BOUND: usize = 16;

/// The cause read after `link`.
///
/// `io::Error` reports the source of the error it wraps, not that error, so a
/// Rustls failure carried inside one is skipped by `source` alone. The wrapped
/// error is read first.
fn next_cause<'a>(link: &'a (dyn Error + 'static)) -> Option<&'a (dyn Error + 'static)> {
    match link
        .downcast_ref::<std::io::Error>()
        .and_then(std::io::Error::get_ref)
    {
        Some(wrapped) => Some(wrapped),
        None => link.source(),
    }
}

/// Whether a Rustls error is among the causes of `error`.
fn has_tls_cause(error: &(dyn Error + 'static)) -> bool {
    std::iter::successors(Some(error), |link| next_cause(*link))
        .take(CAUSE_BOUND)
        .any(|link| link.is::<rustls::Error>())
}

/// The category one Reqwest failure belongs to.
///
/// The facts are read from the most specific to the least. A construction
/// failure never reached a transport. A DNS or TLS failure is also a connect
/// failure, and a timeout can be one, so each is read before `is_connect`. A
/// redirect failure comes only from Reqwest's own hop ceiling. Everything
/// else is a transport failure: an unknown fact is not guessed into a narrower
/// category.
pub fn client_error_projection(error: &reqwest::Error) -> ClientError {
    match error {
        error if error.is_builder() => ClientError::Configuration,
        error if error.is_dns() => ClientError::Dns,
        error if has_tls_cause(error) => ClientError::Tls,
        error if error.is_timeout() => ClientError::Timeout(ClientTimeout::Transport),
        error if error.is_connect() => ClientError::Connect,
        error if error.is_redirect() => ClientError::Redirect(ClientRedirect::HopLimit),
        _ => ClientError::Transport,
    }
}

/// The client failure one refused collection returns.
///
/// The client arms no quiet interval of its own, so a quiet collection is a
/// transport timer like any other.
pub(super) fn collection_failure(failure: &CollectionFailure) -> RuntimeError {
    RuntimeError::Client(match failure {
        CollectionFailure::Crossed { ceiling } => ClientError::ResponseTooLarge { limit: *ceiling },
        CollectionFailure::Quiet(_) => ClientError::Timeout(ClientTimeout::Transport),
        CollectionFailure::Read(error) => client_error_projection(error),
    })
}
