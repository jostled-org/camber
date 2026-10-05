//! The DNS-01 certificate cache: one validated generation per directory.
//!
//! A generation is published as the bundle `certificate.pem` and only through
//! [`publish_generation`]. The reader prefers that bundle whenever it exists. A
//! corrupt bundle is a cache error, never permission to read an older pair. A
//! legacy `cert.pem`/`key.pem` pair is read only when no bundle exists, and it
//! is validated and migrated to a bundle before it is used. The retired expiry
//! stamp is never read.
//!
//! An admitted owner's read is one reported `CacheRead`, however many files it
//! opens, and a legacy migration is one reported `CacheWrite`.

use std::path::Path;
use std::sync::Arc;

use super::certificate::{Generation, Refusal, bundle_of, unix_now, validate};
use super::failure::failure;
use super::publication::{Fault, write_private_file};
use crate::integration_lifecycle::NestedTerminals;
use crate::{IntegrationError, IntegrationFailure, IntegrationOperation, Retryability};

/// The one file a generation is published as.
const BUNDLE: &str = "certificate.pem";
/// The legacy certificate half, read only when no bundle exists.
const LEGACY_CERT: &str = "cert.pem";
/// The legacy key half, read only when no bundle exists.
const LEGACY_KEY: &str = "key.pem";

/// Read the generation cached in `dir` for `domains` at Unix time `now`.
///
/// `Ok(None)` means no bundle and no legacy pair exist.
///
/// # Errors
///
/// `CacheRead` with `InvalidCertificate` for a bundle or pair that fails
/// validation, or for half a legacy pair. `CacheRead` for a file that could not
/// be read. `CacheWrite` when a valid legacy pair could not be migrated.
fn read_generation(
    dir: &Path,
    domains: &[Arc<str>],
    now: i64,
) -> Result<Option<Generation>, IntegrationError> {
    inspect(dir, domains, now)?.resolve(|generation| publish_generation(dir, generation, None))
}

/// Read the generation cached in `dir` for `domains` now, as one reported
/// `CacheRead` of `terminals`' instance, and migrate a legacy pair as one
/// reported `CacheWrite`.
///
/// # Errors
///
/// As [`read_generation`].
pub(super) fn read_reported(
    dir: &Path,
    domains: &[Arc<str>],
    terminals: &NestedTerminals,
) -> Result<Option<Generation>, IntegrationError> {
    terminals
        .run(IntegrationOperation::CacheRead, || {
            cache_io(|| inspect(dir, domains, unix_now()))
        })?
        .resolve(|generation| {
            terminals.run(IntegrationOperation::CacheWrite, || {
                cache_io(|| publish_generation(dir, generation, None))
            })
        })
}

/// What a cache directory holds for one domain set, read and validated with
/// nothing written.
enum Found {
    /// No bundle and no legacy pair.
    Absent,
    /// A valid bundle.
    Bundle(Generation),
    /// A valid legacy pair, not yet migrated.
    Legacy(Generation),
}

impl Found {
    /// The generation to use, migrating a legacy pair through `migrate`
    /// before it is used.
    fn resolve(
        self,
        migrate: impl FnOnce(&Generation) -> Result<(), IntegrationError>,
    ) -> Result<Option<Generation>, IntegrationError> {
        match self {
            Self::Absent => Ok(None),
            Self::Bundle(generation) => Ok(Some(generation)),
            Self::Legacy(generation) => migrate(&generation).map(|()| Some(generation)),
        }
    }
}

/// Read and validate what `dir` holds for `domains` at Unix time `now`,
/// preferring the bundle whenever it exists.
fn inspect(dir: &Path, domains: &[Arc<str>], now: i64) -> Result<Found, IntegrationError> {
    match read_optional(&dir.join(BUNDLE))? {
        Some(bundle) => admit_cached(bundle, domains, now).map(Found::Bundle),
        None => inspect_legacy(dir, domains, now),
    }
}

/// Read the generation cached in `dir` for `domains` now, off the async poll
/// path.
///
/// # Errors
///
/// As [`read_generation`].
pub(super) fn read_cached(
    dir: &Path,
    domains: &[Arc<str>],
) -> Result<Option<Generation>, IntegrationError> {
    cache_io(|| read_generation(dir, domains, unix_now()))
}

/// Whether a cache read calls for renewal.
///
/// No cached generation, and a generation the reader refuses, both read as
/// "renew": the alternative is skipping a renewal the certificate needs. A
/// refusal is reported at warn level, because that answer repeats on every
/// renewal pass.
pub(super) fn renewal_needed<E: std::fmt::Display>(cached: Result<Option<Generation>, E>) -> bool {
    match cached {
        Ok(Some(generation)) => generation.renewal_due(unix_now()),
        Ok(None) => true,
        Err(error) => {
            tracing::warn!(%error, "dns01 acme: cached certificate unusable; renewing");
            true
        }
    }
}

/// Run one blocking cache-file operation without stalling the async poll path.
///
/// Every caller is reachable from public API a user may drive from either
/// runtime flavor, or from no runtime at all, so the flavor check is what
/// keeps a few filesystem syscalls from becoming a panic raised out of library
/// code. `crate::task::block_in_place` is that check; this name records why the
/// cache reaches for it.
pub(super) fn cache_io<T>(operation: impl FnOnce() -> T) -> T {
    crate::task::block_in_place(operation)
}

/// Publish `generation` as the bundle in `dir`, failing at `fault`.
///
/// # Errors
///
/// `CacheWrite` when any stage fails. A failure before the rename keeps the
/// prior generation.
pub(super) fn publish_generation(
    dir: &Path,
    generation: &Generation,
    fault: Fault,
) -> Result<(), IntegrationError> {
    write_cache_file(dir, BUNDLE, generation.bundle(), fault)
}

/// Replace `file` in the cache directory `dir` with `contents` through the
/// cache's one writer, creating `dir` first, failing at `fault`.
///
/// # Errors
///
/// `CacheWrite` with the typed I/O failure of the first stage that failed.
pub(super) fn write_cache_file(
    dir: &Path,
    file: &str,
    contents: &[u8],
    fault: Fault,
) -> Result<(), IntegrationError> {
    std::fs::create_dir_all(dir)
        .and_then(|()| write_private_file(&dir.join(file), contents, fault))
        .map_err(|error| io_failure(IntegrationOperation::CacheWrite, error))
}

/// The typed refusal of a generation that failed validation during `operation`.
///
/// The refusal's bounded label is logged; the error carries no certificate text.
pub(super) fn invalid_certificate(
    operation: IntegrationOperation,
    refusal: Refusal,
) -> IntegrationError {
    tracing::warn!(
        %operation,
        reason = refusal.label(),
        "dns01 acme: certificate generation refused"
    );
    failure(
        operation,
        IntegrationFailure::InvalidCertificate,
        Retryability::Never,
    )
}

fn admit_cached(
    bundle: Box<[u8]>,
    domains: &[Arc<str>],
    now: i64,
) -> Result<Generation, IntegrationError> {
    validate(bundle, domains, now)
        .map_err(|refusal| invalid_certificate(IntegrationOperation::CacheRead, refusal))
}

/// Read and validate a legacy pair; half a pair is unusable.
fn inspect_legacy(dir: &Path, domains: &[Arc<str>], now: i64) -> Result<Found, IntegrationError> {
    let cert = read_optional(&dir.join(LEGACY_CERT))?;
    let key = read_optional(&dir.join(LEGACY_KEY))?;
    match (cert, key) {
        (None, None) => Ok(Found::Absent),
        (Some(cert), Some(key)) => {
            admit_cached(bundle_of(&cert, &key), domains, now).map(Found::Legacy)
        }
        (Some(_), None) | (None, Some(_)) => Err(invalid_certificate(
            IntegrationOperation::CacheRead,
            Refusal::Unusable,
        )),
    }
}

/// Read `path` whole, or report it absent.
///
/// # Errors
///
/// `CacheRead` with the typed I/O failure when `path` exists but cannot be
/// read.
pub(super) fn read_optional(path: &Path) -> Result<Option<Box<[u8]>>, IntegrationError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes.into_boxed_slice())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_failure(IntegrationOperation::CacheRead, error)),
    }
}

/// The typed failure of a cache file operation, keeping the I/O error.
pub(super) fn io_failure(
    operation: IntegrationOperation,
    error: std::io::Error,
) -> IntegrationError {
    let (failure_kind, retryability) = match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            (IntegrationFailure::PermissionDenied, Retryability::Never)
        }
        _ => (IntegrationFailure::Unavailable, Retryability::Safe),
    };
    failure(operation, failure_kind, retryability).with_source(Arc::new(error))
}
