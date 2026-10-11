//! The one terminal record of an outbound operation.
//!
//! Every value recorded here is a closed label, a number, or the host the call
//! began at. The record holds no URL, no header, no body, and no error text, so
//! the event can be published where the request cannot.

use super::super::method::Method as LocalMethod;
use super::super::record::status_to_label;
use super::error::ClientError;
use super::response::ClientResponse;
use crate::RuntimeError;
use reqwest::{Method, Url};
use std::time::Instant;

/// The label every extension method is recorded under.
///
/// A method is any token the application writes, so its own text is an
/// unbounded label.
const EXTENSION_METHOD: &str = "OTHER";

/// The label of TRACE, which the client sends and the router does not route.
const TRACE_METHOD: &str = "TRACE";

/// The bounded label `method` is recorded under.
pub(super) fn method_label(method: &Method) -> &'static str {
    match LocalMethod::from_reqwest(method) {
        Some(routed) => routed.as_str(),
        None if *method == Method::TRACE => TRACE_METHOD,
        None => EXTENSION_METHOD,
    }
}

/// How one operation ended.
#[derive(Clone, Copy)]
enum Outcome {
    Response,
    Configuration,
    NoRuntime,
    InvalidRequest,
    Dns,
    Connect,
    Tls,
    Transport,
    Timeout,
    Redirect,
    ResponseTooLarge,
    Cancelled,
    /// The pending call was dropped. No returned value records this outcome.
    Abandoned,
}

impl Outcome {
    /// The fixed word this outcome is recorded as.
    const fn label(self) -> &'static str {
        match self {
            Self::Response => "response",
            Self::Configuration => "configuration",
            Self::NoRuntime => "no-runtime",
            Self::InvalidRequest => "invalid-request",
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Tls => "tls",
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::Redirect => "redirect",
            Self::ResponseTooLarge => "response-too-large",
            Self::Cancelled => "cancelled",
            Self::Abandoned => "abandoned",
        }
    }
}

/// The outcome of an operation that returned `category`.
///
/// The nested categories are dropped: the event says which boundary ended the
/// call, and the returned error says which part of it. A status and a decoding
/// failure come from a local conversion of an answer, after the operation
/// returned, so no operation ends with one. Each is read as the least specific
/// failure and is not guessed into a narrower outcome.
const fn category_outcome(category: ClientError) -> Outcome {
    match category {
        ClientError::Configuration => Outcome::Configuration,
        ClientError::InvalidRequest(_) | ClientError::Encode => Outcome::InvalidRequest,
        ClientError::Dns => Outcome::Dns,
        ClientError::Connect => Outcome::Connect,
        ClientError::Tls => Outcome::Tls,
        ClientError::Timeout(_) => Outcome::Timeout,
        ClientError::Redirect(_) => Outcome::Redirect,
        ClientError::ResponseTooLarge { .. } => Outcome::ResponseTooLarge,
        ClientError::Transport | ClientError::Status(_) | ClientError::Decode(_) => {
            Outcome::Transport
        }
    }
}

/// The outcome of an operation that returned `error`.
///
/// An operation returns a client category, a cancellation, or a missing
/// runtime. Any other failure is read as the least specific one.
fn failure_outcome(error: &RuntimeError) -> Outcome {
    match error {
        RuntimeError::Client(category) => category_outcome(*category),
        RuntimeError::Cancelled => Outcome::Cancelled,
        RuntimeError::NoRuntime => Outcome::NoRuntime,
        _ => Outcome::Transport,
    }
}

/// The outcome a completion event records for an operation that returned
/// `error`.
///
/// Semver-unsupported. It reads the error and changes nothing.
#[doc(hidden)]
pub fn client_terminal_outcome(error: &RuntimeError) -> &'static str {
    failure_outcome(error).label()
}

/// The finalization guard of one started operation.
///
/// The operation's first poll creates it, and its drop records the one
/// `http.client.completed` event. A call that returns settles the outcome
/// first. A pending call that is dropped never does, so its guard still holds
/// the outcome it began with and records the abandonment. No task is spawned
/// and nothing is recorded later.
pub(super) struct Completion {
    method: &'static str,
    /// The host of the URL the call began at. No redirect changes it.
    host: Box<str>,
    entered: Instant,
    /// The status of the final response head. A head released for a retry or
    /// a redirect is not final.
    status: Option<u16>,
    /// The sends the transport was given.
    attempts: usize,
    redirects: usize,
    outcome: Outcome,
}

impl Completion {
    /// Begin the record of a call to `initial` at its first poll.
    pub(super) fn begin(method: &Method, initial: &Url) -> Self {
        Self {
            method: method_label(method),
            host: initial.host_str().unwrap_or_default().into(),
            entered: Instant::now(),
            status: None,
            attempts: 0,
            redirects: 0,
            outcome: Outcome::Abandoned,
        }
    }

    /// Count one send given to the transport.
    pub(super) fn sent(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Count one followed redirect.
    pub(super) fn followed(&mut self) {
        self.redirects = self.redirects.saturating_add(1);
    }

    /// Record the status of the head the operation collects as its answer.
    pub(super) fn final_head(&mut self, status: u16) {
        self.status = Some(status);
    }

    /// Record how the operation returned.
    pub(super) fn settle(&mut self, result: &Result<ClientResponse, RuntimeError>) {
        self.outcome = match result {
            Ok(_) => Outcome::Response,
            Err(error) => failure_outcome(error),
        };
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        tracing::info!(
            name: "http.client.completed",
            method = self.method,
            host = &*self.host,
            status = status_to_label(self.status),
            elapsed_ms = self.entered.elapsed().as_millis(),
            attempts = self.attempts,
            redirects = self.redirects,
            outcome = self.outcome.label(),
            "outbound request completed"
        );
    }
}
