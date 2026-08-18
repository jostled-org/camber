use std::borrow::Cow;
use std::sync::Arc;

/// The label a request whose method Camber cannot name is recorded under.
///
/// Request recording and metrics label methods with `&'static str`, and a
/// method outside the route enum has no such name. Recording every one of them
/// under this keeps the refusal countable without turning a peer's arbitrary
/// method text into an unbounded label.
const UNNAMEABLE_METHOD: &str = "UNKNOWN";

/// The method one accepted request arrived with.
///
/// [`Method`] is the closed set Camber routes on. A peer may send anything
/// else, and that request still needs an answer, a record, and a rejection
/// context that repeats what it actually sent. Held apart from `Method` so the
/// routing enum stays closed and only the unroutable case pays for a string.
#[derive(Clone, Debug)]
pub(super) enum RequestMethod {
    /// One of the methods Camber routes on. Nothing is allocated.
    Known(Method),
    /// A method outside the route enum, kept exactly as received.
    ///
    /// Shared rather than boxed because the identity carrying it is cloned into
    /// every terminal that may have to name this request.
    Unnameable(Arc<str>),
}

impl RequestMethod {
    /// Name the method a hyper request arrived with.
    pub(super) fn from_hyper(method: &hyper::Method) -> Self {
        Method::from_hyper(method)
            .map_or_else(|| Self::Unnameable(Arc::from(method.as_str())), Self::Known)
    }

    /// The method Camber can route on, when it can route on this one.
    pub(super) fn routable(&self) -> Option<Method> {
        match self {
            Self::Known(method) => Some(*method),
            Self::Unnameable(_) => None,
        }
    }

    /// The method exactly as the peer sent it.
    pub(super) fn as_str(&self) -> &str {
        match self {
            Self::Known(method) => method.as_str(),
            Self::Unnameable(text) => text,
        }
    }

    /// The method exactly as the peer sent it, without copying a known one.
    pub(super) fn to_cow(&self) -> Cow<'static, str> {
        match self {
            Self::Known(method) => Cow::Borrowed(method.as_str()),
            Self::Unnameable(text) => Cow::Owned(text.to_string()),
        }
    }

    /// Every name a method label may carry.
    ///
    /// The routable methods plus the one sentinel every other method is
    /// recorded under, so the label vocabulary stays closed however many
    /// methods a peer invents.
    pub(super) fn vocabulary() -> Box<[&'static str]> {
        Method::ALL
            .map(Method::as_str)
            .into_iter()
            .chain([UNNAMEABLE_METHOD])
            .collect()
    }

    /// The bounded label this method is recorded and counted under.
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Known(method) => method.as_str(),
            Self::Unnameable(_) => UNNAMEABLE_METHOD,
        }
    }
}

/// HTTP method for route matching and request identification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Method {
    /// GET
    Get,
    /// POST
    Post,
    /// PUT
    Put,
    /// DELETE
    Delete,
    /// PATCH
    Patch,
    /// HEAD
    Head,
    /// OPTIONS
    Options,
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unknown HTTP method string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseMethodError;

impl std::fmt::Display for ParseMethodError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("unknown HTTP method")
    }
}

impl std::error::Error for ParseMethodError {}

impl std::str::FromStr for Method {
    type Err = ParseMethodError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or(ParseMethodError)
    }
}

impl Method {
    /// Every method Camber routes on.
    ///
    /// Named exhaustively so the recorded label vocabulary is published from
    /// the enum rather than transcribed beside it.
    pub(super) const ALL: [Self; 7] = [
        Self::Get,
        Self::Post,
        Self::Put,
        Self::Delete,
        Self::Patch,
        Self::Head,
        Self::Options,
    ];

    /// Number of HTTP method variants.
    pub(super) const COUNT: usize = 7;

    /// Parse from a method string (e.g. "GET", "POST").
    pub(super) fn parse(s: &str) -> Option<Self> {
        match s {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            "HEAD" => Some(Self::Head),
            "OPTIONS" => Some(Self::Options),
            _ => None,
        }
    }

    /// Convert from a hyper Method.
    pub(super) fn from_hyper(m: &hyper::Method) -> Option<Self> {
        Self::parse(m.as_str())
    }

    /// Convert from a reqwest Method.
    pub(super) fn from_reqwest(m: &reqwest::Method) -> Option<Self> {
        Self::parse(m.as_str())
    }

    /// Return the uppercase string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }

    /// Stable index (0..6) for array-based dispatch.
    ///
    /// `const` so the compiler, not a reader, holds the index space to the
    /// variant set — see the assertion below this block.
    pub(super) const fn ordinal(self) -> usize {
        match self {
            Self::Get => 0,
            Self::Post => 1,
            Self::Put => 2,
            Self::Delete => 3,
            Self::Patch => 4,
            Self::Head => 5,
            Self::Options => 6,
        }
    }
}

/// The last ordinal fills the last slot of a `[_; COUNT]` array.
///
/// [`Method::COUNT`] and [`Method::ordinal`] are hand-written facts that index
/// fixed-size arrays in the trie. Stated to the compiler so a count raised
/// without an ordinal to fill it, or a final ordinal moved out from under the
/// count, stops the build instead of leaving a dead slot behind.
const _: () = assert!(Method::Options.ordinal() + 1 == Method::COUNT);
