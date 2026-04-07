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
    pub(super) fn ordinal(self) -> usize {
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
