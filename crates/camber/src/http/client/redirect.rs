//! Whether one redirect answer may be followed, and what the next send is.
//!
//! Four questions are answered here and nowhere else: which answers ask for a
//! redirect, where one `Location` leads, what a status does to the method and
//! the body, and whether an allowance has a hop left. The operation that acts
//! on them holds no second copy of any.
//!
//! Same-origin policy admits only the initial origin. Cross-origin policy adds
//! an explicit allowlist and refuses HTTPS downgrades. Each admitted hop says
//! whether it changes origin. The exchange that applies it removes the
//! credentials and replaces the transport on that change.

use super::description::{exact_origin, parsed_url, same_origin, sendable};
use super::error::{ClientError, ClientInput, ClientRedirect, ClientRedirectPolicy};
use super::replay::{RetryPolicy, client_resendable};
use crate::RuntimeError;
use reqwest::header::{HeaderMap, LOCATION};
use reqwest::{Method, Url};
use std::num::NonZeroUsize;
use std::sync::Arc;

/// Which redirect answers a request follows.
///
/// A client holds the default for its requests: set it with
/// [`Client::redirect_policy`](super::Client::redirect_policy). One request
/// replaces its own with
/// [`ClientRequest::redirect_policy`](super::ClientRequest::redirect_policy).
///
/// A followed redirect is one more send. Camber compares scheme, host, and
/// effective port. Destination egress policy checks resolved addresses separately.
/// A change of origin permanently removes credentials and sensitive headers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
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
    /// Follow redirects to the initial origin and an explicit allowlist.
    /// Construct with [`Self::cross_origin`]. Credentials are removed permanently
    /// on each origin change. HTTPS never redirects to HTTP.
    ///
    /// The method and body rules are those of [`Self::SameOrigin`]. A 307 or
    /// 308 that repeats the body sends it to the allowlisted origin.
    CrossOrigin(CrossOriginRedirects),
}

/// A validated, finite cross-origin redirect allowance.
///
/// The allowlist is sorted and has no repeated origin, so two allowances
/// with the same hop bound and the same origins are equal in any order.
/// `Debug` prints the hop bound and the origins. An origin holds no
/// credentials, path, or query, so it is not secret.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CrossOriginRedirects {
    max_hops: NonZeroUsize,
    /// Bare origins: a scheme, a host, an optional port, and the root path.
    origins: Arc<[Url]>,
}

impl CrossOriginRedirects {
    /// The most redirects one call follows.
    #[must_use]
    pub const fn max_hops(&self) -> NonZeroUsize {
        self.max_hops
    }

    /// The allowlisted origins in sorted order, as the URL parser serialized
    /// them: `https://cdn.example:8443`. A name is in lower case, an IDN is in
    /// its ASCII form, and a default port is not written.
    pub fn origins(&self) -> impl ExactSizeIterator<Item = &str> + '_ {
        self.origins.iter().map(origin_text)
    }

    /// Whether a hop from `current` to `resolved` stays inside this allowance:
    /// it does not leave HTTPS for HTTP, and it reaches the initial origin,
    /// which `returns` says, or an allowlisted one.
    fn admits(&self, returns: bool, current: &Url, resolved: &Url) -> Result<(), ClientRedirect> {
        let downgrade = current.scheme() == "https" && resolved.scheme() == "http";
        let permitted = returns
            || self
                .origins
                .iter()
                .any(|origin| same_origin(origin, resolved));
        match (downgrade, permitted) {
            (true, _) => Err(ClientRedirect::Downgrade),
            (false, true) => Ok(()),
            (false, false) => Err(ClientRedirect::OriginChanged),
        }
    }
}

/// One bare origin without the root path its URL serializes.
fn origin_text(origin: &Url) -> &str {
    let text = origin.as_str();
    text.strip_suffix('/').unwrap_or(text)
}

impl std::fmt::Debug for CrossOriginRedirects {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossOriginRedirects")
            .field("max_hops", &self.max_hops)
            .field("origins", &OriginList(&self.origins))
            .finish()
    }
}

/// The `Debug` form of an allowlist: its origin texts.
struct OriginList<'a>(&'a [Url]);

impl std::fmt::Debug for OriginList<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(origin_text))
            .finish()
    }
}

/// The refusal one invalid cross-origin argument returns.
const fn refused(reason: ClientRedirectPolicy) -> RuntimeError {
    RuntimeError::Client(ClientError::InvalidRequest(ClientInput::RedirectPolicy(
        reason,
    )))
}

impl RedirectPolicy {
    /// Permit exact HTTP(S) origins under one positive hop bound.
    ///
    /// Each origin is a scheme, a host, and an optional port, with at most a
    /// root path. It has no credentials, query, or fragment. The initial
    /// origin is always permitted. Egress policy still checks every
    /// destination, including returns to an earlier origin.
    ///
    /// Origins compare by scheme, normalized host, and effective port. Case,
    /// IDN spelling, and a default port do not make another origin. A root
    /// dot does: `cdn.example.` and `cdn.example` are two origins here,
    /// although an egress exception treats them as one.
    ///
    /// On each origin change, Camber removes `Authorization`, `Cookie`, and
    /// every header set with `sensitive_header`. A secret set with plain
    /// `header`, such as `X-Api-Key`, is not marked and goes to the next
    /// origin. Set such secrets with `sensitive_header`.
    ///
    /// ```
    /// use camber::http::{self, RedirectPolicy};
    /// # fn main() -> Result<(), camber::RuntimeError> {
    /// let follow = RedirectPolicy::cross_origin(3, ["https://downloads.example"])?;
    /// let client = http::client().redirect_policy(follow);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns `ClientInput::RedirectPolicy` with the reason: `ZeroHops` for
    /// a zero hop bound, `Origin(index)` for the first invalid entry, or
    /// `EmptyAllowlist` for no entry. The reason never repeats an entry's
    /// text.
    pub fn cross_origin(
        max_hops: usize,
        origins: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, RuntimeError> {
        let max_hops =
            NonZeroUsize::new(max_hops).ok_or(refused(ClientRedirectPolicy::ZeroHops))?;
        let mut origins = origins
            .into_iter()
            .enumerate()
            .map(|(index, origin)| {
                exact_origin(origin.as_ref()).ok_or(refused(ClientRedirectPolicy::Origin(index)))
            })
            .collect::<Result<Vec<Url>, RuntimeError>>()?;
        origins.sort_unstable();
        origins.dedup();
        match origins.is_empty() {
            true => Err(refused(ClientRedirectPolicy::EmptyAllowlist)),
            false => Ok(Self::CrossOrigin(CrossOriginRedirects {
                max_hops,
                origins: origins.into(),
            })),
        }
    }

    /// Whether a hop from `current` to `resolved` stays inside the origins
    /// this policy permits. Every policy but `CrossOrigin` permits only the
    /// initial origin.
    fn admits(&self, initial: &Url, current: &Url, resolved: &Url) -> Result<(), ClientRedirect> {
        match (self, same_origin(initial, resolved)) {
            (Self::CrossOrigin(allowance), returns) => allowance.admits(returns, current, resolved),
            (Self::None | Self::SameOrigin { .. }, true) => Ok(()),
            (Self::None | Self::SameOrigin { .. }, false) => Err(ClientRedirect::OriginChanged),
        }
    }
}

/// What a followed redirect carries from the request it answers.
pub(super) enum Carried {
    /// The same method, body, and headers.
    Whole,
    /// This method, with no body and no `Content-Type`.
    Bodiless(Method),
}

/// One admitted destination, and whether it changes origin.
pub(super) struct Destination {
    /// The admitted URL, without a fragment.
    pub(super) url: Url,
    /// Whether `url` is at another origin than the send it answers. Such a
    /// hop needs a fresh transport and loses the request's credentials.
    pub(super) new_origin: bool,
}

/// One authorized redirect: the next send, and the allowance it spends.
pub(super) struct Hop {
    pub(super) destination: Destination,
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
pub(super) fn redirect_location<'a>(
    policy: &RedirectPolicy,
    status: u16,
    headers: &'a HeaderMap,
) -> Option<Result<&'a [u8], ClientRedirect>> {
    let mut locations = headers.get_all(LOCATION).iter();
    match (policy, status, locations.next(), locations.next()) {
        (
            RedirectPolicy::SameOrigin { .. } | RedirectPolicy::CrossOrigin(_),
            301..=303 | 307 | 308,
            Some(only),
            None,
        ) => Some(Ok(only.as_bytes())),
        (
            RedirectPolicy::SameOrigin { .. } | RedirectPolicy::CrossOrigin(_),
            301..=303 | 307 | 308,
            Some(_),
            Some(_),
        ) => Some(Err(ClientRedirect::InvalidLocation)),
        _ => None,
    }
}

/// The destination one `Location` value sends the call to under `policy`, or
/// the refusal.
///
/// `location` is resolved against `current`, the URL of the send it answers.
/// The policy then admits the origin: the initial origin always, and an
/// allowlisted one under `CrossOrigin`, which also refuses HTTPS to HTTP. Only
/// then must the URL be sendable, so a destination with a user name or a
/// password is refused as invalid. The admitted URL has no fragment.
pub(super) fn redirect_destination(
    policy: &RedirectPolicy,
    initial: &Url,
    current: &Url,
    location: &[u8],
) -> Result<Destination, ClientRedirect> {
    let resolved = std::str::from_utf8(location)
        .ok()
        .and_then(|text| parsed_url(text, Some(current)))
        .ok_or(ClientRedirect::InvalidLocation)?;
    policy.admits(initial, current, &resolved)?;
    let new_origin = !same_origin(current, &resolved);
    sendable(resolved)
        .map(|url| Destination { url, new_origin })
        .ok_or(ClientRedirect::InvalidLocation)
}

/// The URL one `Location` value sends a call under `policy` to, or the
/// refusal.
///
/// The call began at `initial` and is now at `current`. The decision is
/// `redirect_destination`'s: the origin must be the initial one or, under
/// `CrossOrigin`, an allowlisted one with no HTTPS-to-HTTP step. A change of
/// scheme is a change of origin. The admitted URL has no fragment and passes
/// the checks a request's own target passes, so a destination with a user
/// name or a password is refused.
///
/// Semver-unsupported. It reads its arguments and changes nothing.
#[doc(hidden)]
pub fn client_redirect_destination(
    policy: &RedirectPolicy,
    initial: &Url,
    current: &Url,
    location: &[u8],
) -> Result<Url, ClientRedirect> {
    redirect_destination(policy, initial, current, location).map(|destination| destination.url)
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
pub fn client_next_redirect(policy: &RedirectPolicy, followed: usize) -> Option<usize> {
    next_redirect(policy, followed)
}

pub(super) fn next_redirect(policy: &RedirectPolicy, followed: usize) -> Option<usize> {
    match policy {
        RedirectPolicy::None => None,
        RedirectPolicy::SameOrigin { max_hops }
        | RedirectPolicy::CrossOrigin(CrossOriginRedirects { max_hops, .. }) => followed
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
