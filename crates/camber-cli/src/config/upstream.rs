//! Shape checks for the proxy upstream URL and its health-check path.
//!
//! Each check returns a reason that never repeats the checked value: a proxy
//! URL may carry credentials, and a diagnostic must not print them.

use super::authority::check_upstream_authority;

/// Check a proxy URL: `http` or `https`, an authority without credentials,
/// and an optional path prefix. No query and no fragment.
pub(super) fn check_proxy_url(url: &str) -> Result<(), &'static str> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("has no http:// or https:// scheme")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    match () {
        _ if !is_http_scheme(scheme) => Err("must use the http or https scheme"),
        _ if rest.contains('?') => Err("must not have a query"),
        _ if rest.contains('#') => Err("must not have a fragment"),
        _ if authority.is_empty() => Err("has no authority"),
        _ if authority.contains('@') => Err("must not carry credentials"),
        _ if has_space_or_control(path) => Err("has whitespace or control characters"),
        _ => check_upstream_authority(authority),
    }
}

/// Check a health-check path: absolute, with no authority and no fragment.
pub(super) fn check_health_path(path: &str) -> Result<(), &'static str> {
    match () {
        _ if path.is_empty() => Err("is empty"),
        _ if !path.starts_with('/') => Err("must be an absolute path"),
        _ if path.starts_with("//") => Err("must not name an authority"),
        _ if path.contains('#') => Err("must not have a fragment"),
        _ if has_space_or_control(path) => Err("has whitespace or control characters"),
        _ => Ok(()),
    }
}

fn is_http_scheme(scheme: &str) -> bool {
    scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")
}

fn has_space_or_control(text: &str) -> bool {
    text.chars()
        .any(|character| character.is_whitespace() || character.is_control())
}
