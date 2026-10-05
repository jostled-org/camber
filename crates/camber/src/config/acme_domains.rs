//! The one validation owner for ACME certificate names.
//!
//! TLS-ALPN-01 and DNS-01 orders, and the CLI names derived from its sites, pass
//! through here before any credential load, cache write, provider request, or
//! listener bind. Each name's shape and canonical form come from
//! [`super::dns_name`]; this owner adds the wildcard, count, and distinctness
//! rules of the selected challenge.

use std::sync::Arc;

use super::dns_name;
use crate::RuntimeError;

/// The most identifiers one DNS-01 order may name.
#[cfg(feature = "dns01")]
const DNS01_DOMAIN_CAP: usize = 100;

/// The ACME challenge an order proves its names with.
///
/// Each variant exists only under the feature that orders with it.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Challenge {
    /// TLS-ALPN challenges answered by the server itself. No wildcards.
    #[cfg(feature = "acme")]
    TlsAlpn01,
    /// TXT records raised through a DNS provider. Wildcards allowed.
    #[cfg(feature = "dns01")]
    Dns01,
}

impl Challenge {
    const fn label(self) -> &'static str {
        match self {
            #[cfg(feature = "acme")]
            Self::TlsAlpn01 => "TLS-ALPN-01",
            #[cfg(feature = "dns01")]
            Self::Dns01 => "DNS-01",
        }
    }

    const fn domain_cap(self) -> Option<usize> {
        match self {
            #[cfg(feature = "acme")]
            Self::TlsAlpn01 => None,
            #[cfg(feature = "dns01")]
            Self::Dns01 => Some(DNS01_DOMAIN_CAP),
        }
    }

    const fn allows_wildcard(self) -> bool {
        match self {
            #[cfg(feature = "acme")]
            Self::TlsAlpn01 => false,
            #[cfg(feature = "dns01")]
            Self::Dns01 => true,
        }
    }
}

/// Validate a complete domain set for `challenge` and return it canonical.
///
/// The whole set is refused on its first invalid name, on a name that repeats
/// another after canonicalization, when it is empty, or when it exceeds the
/// challenge's cap.
pub(crate) fn validate_domains(
    domains: &[Box<str>],
    challenge: Challenge,
) -> Result<Arc<[Arc<str>]>, RuntimeError> {
    check_count(domains.len(), challenge)?;
    let canonical = domains
        .iter()
        .map(|domain| canonical_name(domain, challenge))
        .collect::<Result<Arc<[Arc<str>]>, RuntimeError>>()?;
    check_distinct(&canonical, challenge)?;
    Ok(canonical)
}

fn check_count(count: usize, challenge: Challenge) -> Result<(), RuntimeError> {
    match (count, challenge.domain_cap()) {
        (0, _) => Err(refusal(challenge, "the domain set is empty")),
        (count, Some(cap)) if count > cap => Err(refusal(
            challenge,
            &format!("{count} domains exceed the order cap of {cap}"),
        )),
        _ => Ok(()),
    }
}

/// Refuse a set in which two names share one canonical form.
///
/// Sorted borrows, not a hash set: the check reads each name once more and
/// allocates one pointer per name.
fn check_distinct(canonical: &[Arc<str>], challenge: Challenge) -> Result<(), RuntimeError> {
    let mut sorted: Box<[&str]> = canonical.iter().map(|name| &**name).collect();
    sorted.sort_unstable();
    match sorted.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(refusal(
            challenge,
            &format!("domain {:?} appears more than once", pair[0]),
        )),
        None => Ok(()),
    }
}

/// Check one name and return its canonical form.
fn canonical_name(domain: &str, challenge: Challenge) -> Result<Arc<str>, RuntimeError> {
    let name = dns_name::without_root(domain);
    let (wildcard, base) = match name.strip_prefix(dns_name::WILDCARD_PREFIX) {
        Some(base) => (true, base),
        None => (false, name),
    };
    let checked = match (wildcard, challenge.allows_wildcard()) {
        (true, false) => Err("is a wildcard, which this challenge cannot prove"),
        _ => dns_name::name_shape(name, base),
    };
    match checked {
        Ok(()) => Ok(Arc::from(dns_name::canonical(name))),
        Err(reason) => Err(refusal(challenge, &format!("domain {domain:?} {reason}"))),
    }
}

fn refusal(challenge: Challenge, reason: &str) -> RuntimeError {
    RuntimeError::Config(format!("acme {}: {reason}", challenge.label()).into())
}
