/// Boxed slice of (name, value) cookie pairs.
pub(crate) type CookiePairs = Box<[(Box<str>, Box<str>)]>;

use super::strip_quotes;

/// Parse a Cookie header value into name-value pairs.
///
/// Cookie headers use the format: `name1=value1; name2=value2`.
/// Bare names without `=` are treated as names with empty values.
/// Quoted values have surrounding double quotes stripped.
pub(crate) fn parse_cookies(header_value: &str) -> CookiePairs {
    header_value
        .split(';')
        .filter_map(|pair| parse_one_cookie(pair.trim()))
        .collect()
}

/// Strip semicolons from a cookie field to prevent attribute injection.
pub(crate) fn sanitize_cookie(field: &str) -> String {
    field.replace(';', "")
}

fn parse_one_cookie(trimmed: &str) -> Option<(Box<str>, Box<str>)> {
    let (name, value) = match trimmed.split_once('=') {
        Some((n, v)) => (n.trim(), strip_quotes(v.trim())),
        None => (trimmed, ""),
    };
    match name.is_empty() {
        true => None,
        false => Some((Box::from(name), Box::from(value))),
    }
}

/// SameSite attribute for cookies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    None,
}

impl SameSite {
    fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "Strict",
            Self::Lax => "Lax",
            Self::None => "None",
        }
    }
}

/// Options for setting a cookie with attributes.
///
/// Used with `Response::set_cookie_with` to control cookie behavior.
#[derive(Debug, Clone)]
pub struct CookieOptions {
    path: Option<Box<str>>,
    domain: Option<Box<str>>,
    max_age: Option<u64>,
    same_site: Option<SameSite>,
    is_secure: bool,
    is_http_only: bool,
}

impl Default for CookieOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl CookieOptions {
    pub fn new() -> Self {
        Self {
            path: None,
            domain: None,
            max_age: None,
            same_site: None,
            is_secure: false,
            is_http_only: false,
        }
    }

    pub fn path(mut self, path: &str) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn domain(mut self, domain: &str) -> Self {
        self.domain = Some(domain.into());
        self
    }

    pub fn max_age(mut self, seconds: u64) -> Self {
        self.max_age = Some(seconds);
        self
    }

    pub fn same_site(mut self, value: SameSite) -> Self {
        self.same_site = Some(value);
        self
    }

    pub fn secure(mut self) -> Self {
        self.is_secure = true;
        self
    }

    pub fn http_only(mut self) -> Self {
        self.is_http_only = true;
        self
    }

    /// Format the cookie attributes as a Set-Cookie header value suffix.
    ///
    /// Semicolons in the name, value, path, and domain are stripped to prevent
    /// attribute injection.
    pub(crate) fn format_header(&self, name: &str, value: &str) -> String {
        let mut header = format!("{}={}", sanitize_cookie(name), sanitize_cookie(value));

        if let Some(ref path) = self.path {
            header.push_str("; Path=");
            header.push_str(&sanitize_cookie(path));
        }
        if let Some(ref domain) = self.domain {
            header.push_str("; Domain=");
            header.push_str(&sanitize_cookie(domain));
        }
        if let Some(seconds) = self.max_age {
            header.push_str("; Max-Age=");
            header.push_str(&seconds.to_string());
        }
        if self.is_secure {
            header.push_str("; Secure");
        }
        if self.is_http_only {
            header.push_str("; HttpOnly");
        }
        if let Some(same_site) = self.same_site {
            header.push_str("; SameSite=");
            header.push_str(same_site.as_str());
        }

        header
    }
}
