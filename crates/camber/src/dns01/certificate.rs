//! One certificate generation, admitted only on the evidence of its own leaf.
//!
//! The leaf's SAN set, its key, and its `notBefore` and `notAfter` are the only
//! authority. A generation is one PEM bundle: the certificate chain followed by
//! the private key. Parsing and key matching reuse the TLS store's own
//! `parse_certified_key`, so a generation admitted here is one the store can
//! serve.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rustls::sign::CertifiedKey;
use x509_parser::certificate::X509Certificate;
use x509_parser::extensions::GeneralName;

use crate::config::{WILDCARD_PREFIX, without_root};
use crate::tls::parse_certified_key;

/// Renew once the leaf's own `notAfter` is less than this many seconds away.
///
/// Thirty days. The renewal loop checks every 12 hours, so the window absorbs
/// sixty failed attempts before the served leaf expires.
const RENEWAL_WINDOW_SECS: i64 = 30 * 86_400;

/// A validated cert/key generation.
///
/// Holds the key the TLS store takes, the leaf's own expiry, and the one-file
/// bundle the generation is published as. Only [`validate`] builds one.
pub(super) struct Generation {
    key: CertifiedKey,
    not_after: i64,
    bundle: Box<[u8]>,
}

impl Generation {
    /// The bytes this generation is published as.
    pub(super) fn bundle(&self) -> &[u8] {
        &self.bundle
    }

    /// Whether the leaf's own `notAfter` is inside the renewal window at `now`.
    pub(super) const fn renewal_due(&self, now: i64) -> bool {
        self.not_after.saturating_sub(now) < RENEWAL_WINDOW_SECS
    }

    /// The key the TLS store serves.
    pub(super) fn into_key(self) -> CertifiedKey {
        self.key
    }
}

/// Why a generation was refused. Each value has one bounded label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// No leaf, no key, malformed PEM or DER, or a key from another leaf.
    Unusable,
    /// The leaf's `notBefore` is still ahead.
    NotYetValid,
    /// The leaf's `notAfter` has passed.
    Expired,
    /// A configured domain is not covered by any SAN.
    Uncovered,
}

impl Refusal {
    /// The bounded name this refusal is reported under.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Unusable => "unusable",
            Self::NotYetValid => "not_yet_valid",
            Self::Expired => "expired",
            Self::Uncovered => "uncovered",
        }
    }
}

/// The facts the leaf itself states.
struct LeafFacts {
    not_before: i64,
    not_after: i64,
    dns_names: Box<[Box<str>]>,
}

/// Join a certificate chain and its key into one bundle.
pub(super) fn bundle_of(cert_pem: &[u8], key_pem: &[u8]) -> Box<[u8]> {
    let separator: &[u8] = match cert_pem.last() {
        Some(b'\n') | None => b"",
        Some(_) => b"\n",
    };
    [cert_pem, separator, key_pem].concat().into_boxed_slice()
}

/// Admit `bundle` as a generation for `domains` at Unix time `now`.
///
/// # Errors
///
/// The first [`Refusal`] the bundle earns: an unusable leaf or key, a validity
/// window that does not contain `now`, or a configured domain no SAN covers.
pub(super) fn validate(
    bundle: Box<[u8]>,
    domains: &[Arc<str>],
    now: i64,
) -> Result<Generation, Refusal> {
    let key = parse_certified_key(&bundle, &bundle).map_err(|_| Refusal::Unusable)?;
    let facts = leaf_facts(&key)?;
    check_window(&facts, now)?;
    check_coverage(&facts.dns_names, domains)?;
    Ok(Generation {
        key,
        not_after: facts.not_after,
        bundle,
    })
}

/// Read the validity window and DNS SANs off the key's leaf.
fn leaf_facts(key: &CertifiedKey) -> Result<LeafFacts, Refusal> {
    let leaf = key.end_entity_cert().map_err(|_| Refusal::Unusable)?;
    let (_, certificate) =
        x509_parser::parse_x509_certificate(leaf.as_ref()).map_err(|_| Refusal::Unusable)?;
    let validity = certificate.validity();
    Ok(LeafFacts {
        not_before: validity.not_before.timestamp(),
        not_after: validity.not_after.timestamp(),
        dns_names: dns_names(&certificate)?,
    })
}

/// Every DNS name in the leaf's SAN extension; none when it has no extension.
fn dns_names(certificate: &X509Certificate<'_>) -> Result<Box<[Box<str>]>, Refusal> {
    match certificate.subject_alternative_name() {
        Ok(Some(extension)) => Ok(extension
            .value
            .general_names
            .iter()
            .filter_map(dns_name)
            .collect()),
        Ok(None) => Ok(Box::default()),
        Err(_) => Err(Refusal::Unusable),
    }
}

fn dns_name(name: &GeneralName<'_>) -> Option<Box<str>> {
    match name {
        GeneralName::DNSName(dns) => Some(Box::from(*dns)),
        _ => None,
    }
}

/// Refuse a leaf whose inclusive validity window does not contain `now`.
const fn check_window(facts: &LeafFacts, now: i64) -> Result<(), Refusal> {
    match (now < facts.not_before, now > facts.not_after) {
        (true, _) => Err(Refusal::NotYetValid),
        (_, true) => Err(Refusal::Expired),
        (false, false) => Ok(()),
    }
}

/// Refuse a leaf unless every configured domain is covered by one of its SANs.
fn check_coverage(dns_names: &[Box<str>], domains: &[Arc<str>]) -> Result<(), Refusal> {
    let covered = domains
        .iter()
        .all(|domain| dns_names.iter().any(|san| san_covers(san, domain)));
    match covered {
        true => Ok(()),
        false => Err(Refusal::Uncovered),
    }
}

/// Whether `san` covers the canonical `domain` under TLS hostname rules.
///
/// An exact match covers, which is how a wildcard domain is covered: only by
/// the same wildcard. A wildcard SAN covers exactly one extra leftmost label.
/// It never covers the apex or two labels, and it is never a string suffix.
fn san_covers(san: &str, domain: &str) -> bool {
    let san = without_root(san);
    match san.eq_ignore_ascii_case(domain) {
        true => true,
        false => wildcard_covers(san, domain),
    }
}

/// Whether the wildcard SAN `*.base` covers `domain` as one label over `base`.
fn wildcard_covers(san: &str, domain: &str) -> bool {
    let Some(base) = san.strip_prefix(WILDCARD_PREFIX) else {
        return false;
    };
    match domain.split_once('.') {
        Some((label, rest)) => {
            !label.is_empty() && !label.contains('*') && rest.eq_ignore_ascii_case(base)
        }
        None => false,
    }
}

/// The current Unix time in seconds; a clock before the epoch reads as zero.
pub(super) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}
