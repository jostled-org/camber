//! The closed vocabulary of outbound client failures.
//!
//! Every value here is a category or a number. None holds a URL, a header
//! value, a body, or a third-party error, and none has a source. A caller can
//! print one every way an error prints and learn only which boundary failed.

/// Why an outbound client call returned no usable answer.
///
/// Closed and exhaustively matchable: a new failure is a deliberate API
/// change. `source` is always `None`, so a chain walk ends here and cannot
/// reach transport text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ClientError {
    /// The underlying client could not be constructed.
    #[error("client configuration failed")]
    Configuration,
    /// The request description was refused before any send.
    #[error("invalid request: {0}")]
    InvalidRequest(ClientInput),
    /// The request JSON could not be serialized.
    #[error("request encoding failed")]
    Encode,
    /// The target name did not resolve.
    #[error("dns resolution failed")]
    Dns,
    /// The connection was not established, with no more specific typed cause.
    #[error("connection failed")]
    Connect,
    /// A TLS validation or protocol failure ended the connection.
    #[error("tls failure")]
    Tls,
    /// Any other send or response-read failure, including a truncated body.
    #[error("transport failure")]
    Transport,
    /// A deadline expired.
    #[error("timed out: {0}")]
    Timeout(ClientTimeout),
    /// A redirect was refused instead of followed.
    #[error("redirect refused: {0}")]
    Redirect(ClientRedirect),
    /// An explicit status conversion rejected this 400–599 status.
    #[error("status {0}")]
    Status(u16),
    /// A completed response body could not be decoded.
    #[error("response decoding failed: {0}")]
    Decode(ClientDecode),
    /// The response declared or delivered more than `limit` bytes, or its
    /// running total overflowed.
    #[error("response exceeded {limit} bytes")]
    ResponseTooLarge {
        /// The ceiling the collection was refused under.
        limit: usize,
    },
}

/// The part of a request description that was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientInput {
    /// The method is not a permitted HTTP token.
    Method,
    /// The target is not an absolute HTTP or HTTPS URL this client sends to.
    Url,
    /// A header name or value is outside the HTTP grammar.
    Header,
    /// The header is owned by the transport.
    ReservedHeader,
    /// The body is not allowed for this request.
    Body,
    /// The response ceiling is zero.
    ResponseLimit,
}

impl ClientInput {
    /// The fixed name this category prints.
    const fn label(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Url => "url",
            Self::Header => "header",
            Self::ReservedHeader => "reserved header",
            Self::Body => "body",
            Self::ResponseLimit => "response limit",
        }
    }
}

impl std::fmt::Display for ClientInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The deadline an outbound call crossed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientTimeout {
    /// The complete operation deadline.
    Operation,
    /// The deadline of one attempt.
    Attempt,
    /// A transport timer. Reqwest does not say whether connect or a read
    /// expired, so neither is claimed.
    Transport,
}

impl ClientTimeout {
    /// The fixed name this category prints.
    const fn label(self) -> &'static str {
        match self {
            Self::Operation => "operation",
            Self::Attempt => "attempt",
            Self::Transport => "transport",
        }
    }
}

impl std::fmt::Display for ClientTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Why a redirect was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientRedirect {
    /// The `Location` is malformed or repeated, or it holds a user name or a
    /// password.
    InvalidLocation,
    /// The destination is not the initial origin.
    OriginChanged,
    /// The redirect allowance is spent.
    HopLimit,
    /// The method may not be sent again with its body.
    UnsafeMethod,
}

impl ClientRedirect {
    /// The fixed name this category prints.
    const fn label(self) -> &'static str {
        match self {
            Self::InvalidLocation => "invalid location",
            Self::OriginChanged => "origin changed",
            Self::HopLimit => "hop limit",
            Self::UnsafeMethod => "unsafe method",
        }
    }
}

impl std::fmt::Display for ClientRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// The local decoding that failed on a completed response.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClientDecode {
    /// The body is not UTF-8.
    Utf8,
    /// The body is not the requested JSON.
    Json,
}

impl ClientDecode {
    /// The fixed name this category prints.
    const fn label(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Json => "json",
        }
    }
}

impl std::fmt::Display for ClientDecode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
