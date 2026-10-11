//! Whether one redirect answer may be followed, and what the next send is.
//!
//! Four questions are answered here and nowhere else: which answers ask for a
//! redirect, where one `Location` leads, what a status does to the method and
//! the body, and whether an allowance has a hop left. The operation that acts
//! on them holds no second copy of any.
//!
//! The origin a destination is compared against is the origin the call began
//! at. A hop never widens it, so a chain of authorized hops cannot walk a
//! credential to another origin. A refused destination is never repaired by
//! removing headers: the call ends with the refusal.

use super::description::{parsed_url, sendable};
use super::error::ClientRedirect;
use super::replay::{RetryPolicy, client_resendable};
use reqwest::header::{HeaderMap, LOCATION};
use reqwest::{Method, Url};
use std::num::NonZeroUsize;

/// Which redirect answers a request follows.
///
/// A client holds the default for its requests: set it with
/// [`Client::redirect_policy`](super::Client::redirect_policy). One request
/// replaces its own with
/// [`ClientRequest::redirect_policy`](super::ClientRequest::redirect_policy).
///
/// A followed redirect is one more send. It carries the request's headers,
/// credentials included, so it goes only to the origin the call began at.
/// Camber compares the scheme, the host, and the port. It makes no claim about
/// the address a permitted host resolves to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RedirectPolicy {
    /// Follow nothing. Every 3xx answer is returned as the response, with its
    /// `Location` header and its body. This is the default.
    #[default]
    None,
    /// Follow a 301, 302, 303, 307, or 308 answer to the same origin.
    ///
    /// The answer must hold one `Location`. With none, the answer is returned
    /// as the response. A repeated or malformed one, a destination at another
    /// scheme, host, or port, and a destination with a user name are refused
    /// with a [`ClientRedirect`] and nothing more is sent.
    ///
    /// A 301 or 302 is followed for GET and HEAD and refused for every other
    /// method. A 303 is followed as a GET with no body and no `Content-Type`,
    /// and a HEAD stays a HEAD. A 307 or 308 repeats the method and the body,
    /// so it is followed only for a method the request's [`RetryPolicy`] lets
    /// be repeated: GET, HEAD, OPTIONS, PUT, and DELETE, or any method under
    /// `Replayable`. `Never` withholds retries and does not withhold these.
    SameOrigin {
        /// The most redirects one call follows. The count spans the whole
        /// call, and a retry does not return a spent hop.
        max_hops: NonZeroUsize,
    },
}

/// What a followed redirect carries from the request it answers.
pub(super) enum Carried {
    /// The same method, body, and headers.
    Whole,
    /// This method, with no body and no `Content-Type`.
    Bodiless(Method),
}

/// One authorized redirect: the next send, and the allowance it spends.
pub(super) struct Hop {
    /// The destination, at the initial origin and without a fragment.
    pub(super) url: Url,
    pub(super) carried: Carried,
    /// How many redirects the call has followed once this one is.
    pub(super) ordinal: usize,
}

/// The one `Location` of a redirect answer `policy` acts on.
///
/// `None` is an answer the call returns as its response: the policy follows
/// nothing, the status is not one a redirect is followed for, or the answer
/// names no destination. A repeated `Location` names no single destination and
/// is refused.
pub(super) fn redirect_location(
    policy: RedirectPolicy,
    status: u16,
    headers: &HeaderMap,
) -> Option<Result<&[u8], ClientRedirect>> {
    let mut locations = headers.get_all(LOCATION).iter();
    match (policy, status, locations.next(), locations.next()) {
        (RedirectPolicy::SameOrigin { .. }, 301..=303 | 307 | 308, Some(only), None) => {
            Some(Ok(only.as_bytes()))
        }
        (RedirectPolicy::SameOrigin { .. }, 301..=303 | 307 | 308, Some(_), Some(_)) => {
            Some(Err(ClientRedirect::InvalidLocation))
        }
        (RedirectPolicy::None | RedirectPolicy::SameOrigin { .. }, ..) => None,
    }
}

/// The URL one `Location` value sends the call to, or the refusal.
///
/// `location` is resolved against `current`, the URL of the send it answers.
/// The result must have the origin of `initial`, the URL the call began at:
/// the same scheme, the same normalized host, and the same effective port. A
/// change of scheme is a change of origin in both directions. The admitted URL
/// has no fragment and passes the checks a request's own target passes, so a
/// destination with a user name or a password is refused.
///
/// Semver-unsupported. It reads its arguments and changes nothing.
#[doc(hidden)]
pub fn client_redirect_destination(
    initial: &Url,
    current: &Url,
    location: &[u8],
) -> Result<Url, ClientRedirect> {
    let resolved = std::str::from_utf8(location)
        .ok()
        .and_then(|text| parsed_url(text, Some(current)))
        .ok_or(ClientRedirect::InvalidLocation)?;
    match resolved.origin() == initial.origin() {
        true => sendable(resolved).ok_or(ClientRedirect::InvalidLocation),
        false => Err(ClientRedirect::OriginChanged),
    }
}

/// What a redirect with `status` carries from a request with `method`, or the
/// refusal of a method that may not be sent again.
///
/// A 301 or 302 never rewrites a method: a client that turned a POST into a
/// GET would send a request the caller did not describe. A 303 asks for a
/// fetch of another resource, so it carries no body. A 307 or 308 repeats the
/// request whole, which is the question `permission` answers for a retry.
pub(super) fn redirect_carried(
    status: u16,
    method: &Method,
    permission: RetryPolicy,
) -> Result<Carried, ClientRedirect> {
    match (status, method.as_str()) {
        (303, "HEAD") => Ok(Carried::Bodiless(Method::HEAD)),
        (303, _) => Ok(Carried::Bodiless(Method::GET)),
        (301 | 302, "GET" | "HEAD") => Ok(Carried::Whole),
        (301 | 302, _) => Err(ClientRedirect::UnsafeMethod),
        _ if client_resendable(method, permission) => Ok(Carried::Whole),
        _ => Err(ClientRedirect::UnsafeMethod),
    }
}

/// The ordinal of the redirect a follow would spend after `followed` of them,
/// or `None` when `policy` has no hop left.
///
/// The addition is checked, so a full counter is a spent allowance and not a
/// fresh one.
///
/// Semver-unsupported. It reads its arguments and changes nothing.
#[doc(hidden)]
pub fn client_next_redirect(policy: RedirectPolicy, followed: usize) -> Option<usize> {
    match policy {
        RedirectPolicy::None => None,
        RedirectPolicy::SameOrigin { max_hops } => followed
            .checked_add(1)
            .filter(|ordinal| *ordinal <= max_hops.get()),
    }
}

/// How many sends an operation has made: its first, and one for each retry
/// and each redirect.
///
/// `None` when a `usize` cannot hold the count. An operation makes no send it
/// cannot count.
///
/// Semver-unsupported. It reads its arguments and changes nothing.
#[doc(hidden)]
pub fn client_send_total(retries_sent: u32, redirects_followed: usize) -> Option<usize> {
    usize::try_from(retries_sent)
        .ok()
        .and_then(|retries| retries.checked_add(redirects_followed))
        .and_then(|added| added.checked_add(1))
}
