//! The stored head of one outbound answer: its status and its headers.
//!
//! Every outbound answer keeps its head in this one shape, so the copy out of
//! the transport and the rules a caller reads a header by exist once.

use super::super::response::HeaderPair;
use super::error::ClientError;
use crate::RuntimeError;
use reqwest::header::HeaderMap;

/// The statuses a status conversion refuses.
const ERROR_STATUSES: std::ops::RangeInclusive<u16> = 400..=599;

/// One response header as it is stored: its name, and its value's raw bytes.
pub(in crate::http) type HeaderEntry = (Box<str>, Box<[u8]>);

/// Copy a peer's headers out of the transport's map.
///
/// The map yields a repeated name once per value, in the order the values
/// arrived, so the copy keeps both the multiplicity and that order. A value
/// is copied as bytes: one that is not text stays what the peer sent.
fn wire_headers(headers: &HeaderMap) -> Box<[HeaderEntry]> {
    headers
        .iter()
        .map(|(name, value)| (Box::from(name.as_str()), Box::from(value.as_bytes())))
        .collect()
}

/// Copy a canned answer's text headers into the stored shape.
///
/// A canned answer keeps the spelling its author wrote. The stored name is
/// lowercase, as a name copied from the transport is, so a caller reads one
/// spelling from both.
pub(in crate::http) fn text_headers(headers: &[HeaderPair]) -> Box<[HeaderEntry]> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.to_ascii_lowercase().into_boxed_str(),
                Box::from(value.as_bytes()),
            )
        })
        .collect()
}

/// The status and headers of one answer, as a caller reads them.
///
/// It has no `Debug` of its own. [`names`](Self::names) is what an owner's
/// rendering prints, so no rendering prints a header value.
pub(super) struct ResponseHead {
    status: u16,
    /// Never mutated after the copy, so the entries are a boxed slice.
    headers: Box<[HeaderEntry]>,
}

impl ResponseHead {
    pub(super) fn new(status: u16, headers: Box<[HeaderEntry]>) -> Self {
        Self { status, headers }
    }

    /// Copy the head of a transport's answer. The answer keeps its body.
    pub(super) fn of(response: &reqwest::Response) -> Self {
        Self::new(response.status().as_u16(), wire_headers(response.headers()))
    }

    pub(super) fn status(&self) -> u16 {
        self.status
    }

    /// Refuse a 400–599 status as `ClientError::Status`.
    ///
    /// The refusal carries the status number only. Both terminals convert a
    /// status by this one rule.
    pub(super) fn refuse_error_status(&self) -> Result<(), RuntimeError> {
        match ERROR_STATUSES.contains(&self.status) {
            true => Err(RuntimeError::Client(ClientError::Status(self.status))),
            false => Ok(()),
        }
    }

    /// The first value of the header `name`, matched without regard to case.
    pub(super) fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| &**value)
    }

    /// Every header entry, as a lowercase name and the value's raw bytes.
    pub(super) fn headers(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.headers.iter().map(|(name, value)| (&**name, &**value))
    }

    /// The header names a `Debug` rendering lists.
    pub(super) fn names(&self) -> HeaderNames<'_> {
        HeaderNames(&self.headers)
    }
}

/// The header names of one stored head, and none of their values.
pub(super) struct HeaderNames<'a>(&'a [HeaderEntry]);

impl std::fmt::Debug for HeaderNames<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}
