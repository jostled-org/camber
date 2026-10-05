//! The DNS-01 cache's files: self-signed leaves to seed it with, and the
//! bundle's privacy.
//!
//! Every validity window is a pair of offsets from now, in seconds, so a row
//! states where now lies inside or outside the leaf's window.

use crate::integration_rows::Row;
#[cfg(unix)]
use crate::integration_rows::expect_eq;
#[cfg(unix)]
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The file a DNS-01 cache publishes its generation as.
pub const BUNDLE: &str = "certificate.pem";

/// One hour, in seconds.
pub const HOUR: i64 = 3_600;
/// One day, in seconds.
pub const DAY: i64 = 24 * HOUR;

/// One generated leaf and its key, as PEM and as the DER the store receives.
pub struct Leaf {
    /// The leaf certificate, as PEM.
    pub cert_pem: Box<str>,
    /// The leaf certificate, as DER.
    pub der: Box<[u8]>,
    /// The leaf's private key, as PEM.
    pub key_pem: Box<str>,
}

impl Leaf {
    /// The leaf and its key as one cache bundle.
    #[must_use]
    pub fn bundle(&self) -> String {
        format!("{}{}", self.cert_pem, self.key_pem)
    }
}

/// A self-signed leaf for `sans`, valid between the two offsets from now.
///
/// # Errors
///
/// When the clock or rcgen refuses the parameters.
pub fn leaf(sans: &[&str], not_before: i64, not_after: i64) -> Result<Leaf, String> {
    let names: Vec<String> = sans.iter().map(|name| (*name).to_owned()).collect();
    let mut params =
        rcgen::CertificateParams::new(names).map_err(|error| format!("leaf params: {error}"))?;
    validity(&mut params, not_before, not_after)?;
    let key = rcgen::KeyPair::generate().map_err(|error| format!("leaf key: {error}"))?;
    let cert = params
        .self_signed(&key)
        .map_err(|error| format!("leaf signature: {error}"))?;
    Ok(Leaf {
        cert_pem: cert.pem().into_boxed_str(),
        der: Box::from(cert.der().as_ref()),
        key_pem: key.serialize_pem().into_boxed_str(),
    })
}

/// Set the validity window of `params` to the two offsets from now.
///
/// # Errors
///
/// When the clock cannot place either offset.
pub fn validity(params: &mut rcgen::CertificateParams, not_before: i64, not_after: i64) -> Row {
    let epoch = rcgen::date_time_ymd(1970, 1, 1);
    params.not_before = epoch + since_epoch(not_before)?;
    params.not_after = epoch + since_epoch(not_after)?;
    Ok(())
}

/// Now plus `offset` seconds, as a duration since the Unix epoch.
///
/// # Errors
///
/// When the clock reads before the epoch, or the instant falls before it.
pub fn since_epoch(offset: i64) -> Result<Duration, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock: {error}"))?;
    let now = i64::try_from(now.as_secs()).map_err(|error| format!("system clock: {error}"))?;
    let at = u64::try_from(now + offset).map_err(|error| format!("instant: {error}"))?;
    Ok(Duration::from_secs(at))
}

/// Restrict the file at `path` to its owner, as a published bundle is.
///
/// The privacy contract is Unix-only, as the publisher's is.
///
/// # Errors
///
/// When the permissions cannot be set.
#[cfg(unix)]
pub fn make_private(path: &Path) -> Row {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

/// Fail unless only the owner can read or write the file at `path`.
///
/// The privacy contract is Unix-only, as the publisher's is.
///
/// # Errors
///
/// When the file cannot be read, or its mode is not exactly 600.
#[cfg(unix)]
pub fn expect_private(path: &Path) -> Row {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|error| format!("stat {}: {error}", path.display()))?
        .permissions()
        .mode();
    expect_eq("bundle permissions", mode & 0o777, 0o600)
}
