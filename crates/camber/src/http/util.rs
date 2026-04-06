/// Strip surrounding double quotes from a header value (RFC 6265 / RFC 2616).
pub(crate) fn strip_quotes(v: &str) -> &str {
    match v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        true => &v[1..v.len() - 1],
        false => v,
    }
}

/// Map reqwest errors to RuntimeError, detecting timeouts.
pub(crate) fn map_reqwest_error(e: reqwest::Error) -> crate::RuntimeError {
    match e.is_timeout() {
        true => crate::RuntimeError::Timeout,
        false => crate::RuntimeError::Http(e.to_string().into()),
    }
}
