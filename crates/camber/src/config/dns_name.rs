//! The shape of one DNS name, shared by ACME certificate names and the exact
//! host names a configuration declares.
//!
//! A name leaves in canonical form: ASCII lowercase, with no trailing dot.
//! Internationalized names arrive as A-labels; this owner does not convert
//! U-labels.

use crate::RuntimeError;

/// The longest DNS name, in octets, without its trailing dot.
const MAX_NAME_OCTETS: usize = 253;
/// The longest DNS label, in octets.
const MAX_LABEL_OCTETS: usize = 63;
/// The only wildcard form: a whole leftmost label.
#[cfg(any(feature = "acme", feature = "dns01"))]
pub(crate) const WILDCARD_PREFIX: &str = "*.";

/// Check `name` as an exact DNS host name and return its canonical form.
///
/// Performs no I/O. A wildcard, an empty or over-long label, a character
/// outside ASCII letters, digits, and hyphens, a label that begins or ends
/// with a hyphen, and a numeric final label (which parsers read as an IPv4
/// address) all return [`RuntimeError::Config`].
pub fn canonical_dns_name(name: &str) -> Result<Box<str>, RuntimeError> {
    let trimmed = without_root(name);
    let checked = match trimmed.contains('*') {
        true => Err("is a wildcard, not an exact DNS name"),
        false => name_shape(trimmed, trimmed),
    };
    match checked {
        Ok(()) => Ok(canonical(trimmed)),
        Err(reason) => Err(RuntimeError::Config(
            format!("DNS name {name:?} {reason}").into(),
        )),
    }
}

/// `name` without the trailing dot that names the DNS root.
pub(crate) fn without_root(name: &str) -> &str {
    name.strip_suffix('.').unwrap_or(name)
}

/// The canonical spelling of a name whose shape has been checked.
pub(crate) fn canonical(name: &str) -> Box<str> {
    name.to_ascii_lowercase().into_boxed_str()
}

/// Check the shape of `name`, whose labels after any wildcard are `base`.
pub(crate) fn name_shape(name: &str, base: &str) -> Result<(), &'static str> {
    match (name.len() > MAX_NAME_OCTETS, base.is_empty()) {
        (true, _) => return Err("is longer than 253 octets"),
        (false, true) => return Err("has no labels"),
        (false, false) => {}
    }
    base.split('.').try_for_each(label_shape)?;
    // A numeric final label reads as an IPv4 address to TLS and URL parsers.
    match base.rsplit('.').next().map(is_numeric) {
        Some(true) => Err("is an IP address literal, not a DNS name"),
        _ => Ok(()),
    }
}

fn label_shape(label: &str) -> Result<(), &'static str> {
    let permitted = label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    match label {
        "" => Err("has an empty label"),
        _ if label.len() > MAX_LABEL_OCTETS => Err("has a label longer than 63 octets"),
        _ if label.contains('*') => Err("has a wildcard that is not the whole leftmost label"),
        _ if !permitted => Err(
            "has a character outside ASCII letters, digits, and hyphens \
             (internationalized names must be A-labels)",
        ),
        _ if label.starts_with('-') || label.ends_with('-') => {
            Err("has a label that begins or ends with a hyphen")
        }
        _ => Ok(()),
    }
}

fn is_numeric(label: &str) -> bool {
    label.bytes().all(|byte| byte.is_ascii_digit())
}
