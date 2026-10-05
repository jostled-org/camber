#![cfg(feature = "dns01")]

//! 9.T1: the DNS-01 cache admits only a validated certificate generation.
//!
//! Every row enters the public readers, `AcmeDns01::load_cached_cert` and
//! `AcmeDns01::needs_renewal`, over a cache seeded with generated leaves. The
//! leaf's own SAN set, key, `notBefore`, and `notAfter` are the only authority:
//! no row relies on the expiry sidecar, and two rows plant a sidecar that
//! contradicts the leaf. Publication interruption belongs to 9.T3.

#[cfg(unix)]
use crate::dns_cache_files::expect_private;
use crate::dns_cache_files::{DAY, HOUR, Leaf, leaf, since_epoch};
use crate::dns_cleanup_peers::{BUNDLE, generation_expiring, seed_cache};
use crate::integration_rows::{Row, all, assert_verdicts, expect, expect_eq, tempdir};
use camber::dns01::AcmeDns01;
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, RuntimeError};
use std::path::Path;
use tempfile::TempDir;

/// The one configured domain most rows use.
const API: &str = "api.example.com";
/// The one configured domain the cached-generation reuse tests use.
const CACHED: &str = "test.example.com";
const LEGACY_CERT: &str = "cert.pem";
const LEGACY_KEY: &str = "key.pem";
/// The retired synthetic-expiry stamp; never authority.
const EXPIRY_SIDECAR: &str = "expiry";

type CacheRows = fn() -> Vec<Row>;
type CertificatePair = (Box<str>, Box<str>);

#[test]
fn dns_cache_validates_identity_time_and_atomic_migration() {
    let families: [(&str, CacheRows); 5] = [
        ("SAN coverage and key identity", identity_rows),
        ("validity window", validity_rows),
        ("renewal boundary", renewal_rows),
        ("bundle format", bundle_rows),
        ("legacy import", legacy_rows),
    ];
    assert_verdicts(
        "M9 invalid or mixed certificate generation was admitted",
        families
            .iter()
            .flat_map(|&(family, rows)| rows().into_iter().map(move |row| (family, row))),
    );
}

/// A current leaf for [`API`], 60 days from expiry.
fn current_leaf() -> Result<Leaf, String> {
    leaf(&[API], -DAY, 60 * DAY)
}

fn write(dir: &Path, name: &str, contents: &str) -> Row {
    std::fs::write(dir.join(name), contents).map_err(|error| format!("seed {name}: {error}"))
}

fn write_bundle(dir: &Path, cert_pem: &str, key_pem: &str) -> Row {
    write(dir, BUNDLE, &format!("{cert_pem}{key_pem}"))
}

fn write_legacy(dir: &Path, cert_pem: &str, key_pem: &str) -> Row {
    all([
        write(dir, LEGACY_CERT, cert_pem),
        write(dir, LEGACY_KEY, key_pem),
    ])
}

/// A sidecar stamp claiming expiry `offset` seconds from now.
fn write_sidecar(dir: &Path, offset: i64) -> Row {
    let stamp = since_epoch(offset)?.as_secs();
    write(dir, EXPIRY_SIDECAR, &stamp.to_string())
}

fn owner(dir: &Path, domains: &[&str]) -> AcmeDns01 {
    AcmeDns01::new("camber-test", domains.iter().copied()).cache_dir(dir)
}

fn api_owner(dir: &Path) -> AcmeDns01 {
    owner(dir, &[API])
}

fn named(label: &str, row: Row) -> Row {
    row.map_err(|reason| format!("{label}: {reason}"))
}

/// Fail unless the reader admits exactly `expected`'s leaf.
fn expect_admitted(acme: &AcmeDns01, expected: &Leaf) -> Row {
    match acme.load_cached_cert() {
        Ok(Some(key)) => expect_eq(
            "admitted leaf",
            key.cert.first().map(AsRef::<[u8]>::as_ref),
            Some(&*expected.der),
        ),
        Ok(None) => Err("the generation read as absent".to_owned()),
        Err(error) => Err(format!("the generation was refused with {error:?}")),
    }
}

/// Fail unless the reader refuses the generation as a typed invalid-certificate
/// cache read.
fn expect_invalid(acme: &AcmeDns01) -> Row {
    match acme.load_cached_cert() {
        Ok(Some(_)) => Err("the generation was admitted".to_owned()),
        Ok(None) => Err("the generation read as absent, not as a cache error".to_owned()),
        Err(RuntimeError::Integration(error)) => expect_eq(
            "cache refusal",
            (error.kind(), error.operation(), error.failure()),
            (
                IntegrationKind::Dns01,
                IntegrationOperation::CacheRead,
                IntegrationFailure::InvalidCertificate,
            ),
        ),
        Err(other) => Err(format!("{other:?} is not a typed cache refusal")),
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

// --- SAN coverage and key identity ---------------------------------------

/// Configured domains, leaf SANs, and whether the leaf covers them under TLS
/// hostname rules.
const COVERAGE: [(&str, &[&str], &[&str], bool); 9] = [
    (
        "exact SAN set",
        &["api.example.com", "www.example.com"],
        &["api.example.com", "www.example.com"],
        true,
    ),
    (
        "SAN superset",
        &["api.example.com"],
        &["api.example.com", "other.example.com"],
        true,
    ),
    (
        "one configured domain uncovered",
        &["api.example.com", "www.example.com"],
        &["api.example.com"],
        false,
    ),
    (
        "wildcard SAN covers one label",
        &["api.example.com"],
        &["*.example.com"],
        true,
    ),
    (
        "wildcard SAN does not cover the apex",
        &["example.com"],
        &["*.example.com"],
        false,
    ),
    (
        "wildcard SAN does not cover two labels",
        &["a.b.example.com"],
        &["*.example.com"],
        false,
    ),
    (
        "wildcard SAN is not a string suffix",
        &["badexample.com"],
        &["*.example.com"],
        false,
    ),
    (
        "wildcard domain needs a wildcard SAN",
        &["*.example.com"],
        &["api.example.com"],
        false,
    ),
    (
        "wildcard domain with a wildcard SAN",
        &["*.example.com"],
        &["*.example.com"],
        true,
    ),
];

fn identity_rows() -> Vec<Row> {
    let mut rows: Vec<Row> = COVERAGE
        .iter()
        .map(|&(label, domains, sans, admitted)| {
            named(
                label,
                admission_row(domains, leaf(sans, -DAY, 60 * DAY), admitted),
            )
        })
        .collect();
    rows.push(named("key from another generation", mismatched_key_row()));
    rows
}

/// Publish `generation` as the bundle for `domains`, and expect the reader to
/// admit it or refuse it as invalid.
fn admission_row(domains: &[&str], generation: Result<Leaf, String>, admitted: bool) -> Row {
    let dir = tempdir()?;
    let generation = generation?;
    write_bundle(dir.path(), &generation.cert_pem, &generation.key_pem)?;
    let acme = owner(dir.path(), domains);
    match admitted {
        true => expect_admitted(&acme, &generation),
        false => expect_invalid(&acme),
    }
}

fn mismatched_key_row() -> Row {
    let dir = tempdir()?;
    let certificate = current_leaf()?;
    let other = current_leaf()?;
    write_bundle(dir.path(), &certificate.cert_pem, &other.key_pem)?;
    expect_invalid(&api_owner(dir.path()))
}

// --- validity window -----------------------------------------------------

/// Leaf validity offsets from now, and whether now lies inside them.
const VALIDITY: [(&str, i64, i64, bool); 3] = [
    ("current leaf", -DAY, 60 * DAY, true),
    ("future notBefore", DAY, 90 * DAY, false),
    ("expired notAfter", -90 * DAY, -HOUR, false),
];

fn validity_rows() -> Vec<Row> {
    VALIDITY
        .iter()
        .map(|&(label, not_before, not_after, admitted)| {
            named(
                label,
                admission_row(&[API], leaf(&[API], not_before, not_after), admitted),
            )
        })
        .collect()
}

// --- renewal boundary ----------------------------------------------------

/// Leaf notAfter offset, an optional contradicting sidecar stamp, and whether
/// renewal is due.
const RENEWAL: [(&str, i64, Option<i64>, bool); 6] = [
    ("sixty days remain", 60 * DAY, None, false),
    ("just over thirty days remain", 30 * DAY + HOUR, None, false),
    ("just under thirty days remain", 30 * DAY - HOUR, None, true),
    ("leaf already expired", -HOUR, None, true),
    (
        "sidecar promises more than the leaf",
        10 * DAY,
        Some(80 * DAY),
        true,
    ),
    (
        "sidecar promises less than the leaf",
        80 * DAY,
        Some(10 * DAY),
        false,
    ),
];

fn renewal_rows() -> Vec<Row> {
    let mut rows: Vec<Row> = RENEWAL
        .iter()
        .map(|&(label, not_after, sidecar, due)| named(label, renewal_row(not_after, sidecar, due)))
        .collect();
    rows.push(named("missing generation", missing_renewal_row()));
    rows
}

fn renewal_row(not_after: i64, sidecar: Option<i64>, due: bool) -> Row {
    let dir = tempdir()?;
    let generation = leaf(&[API], -60 * DAY, not_after)?;
    write_bundle(dir.path(), &generation.cert_pem, &generation.key_pem)?;
    if let Some(offset) = sidecar {
        write_sidecar(dir.path(), offset)?;
    }
    expect_eq("renewal due", api_owner(dir.path()).needs_renewal(), due)
}

fn missing_renewal_row() -> Row {
    let dir = tempdir()?;
    expect(
        "an empty cache must need renewal",
        api_owner(dir.path()).needs_renewal(),
    )
}

// --- bundle format -------------------------------------------------------

const MALFORMED_PEM: &str =
    "-----BEGIN CERTIFICATE-----\nnot base64 at all!\n-----END CERTIFICATE-----\n";

fn bundle_rows() -> Vec<Row> {
    vec![
        named("malformed PEM bundle", malformed_bundle_row()),
        named("bundle without a key", keyless_bundle_row()),
        named("bundle without a certificate", certless_bundle_row()),
        named(
            "corrupt bundle beside a valid legacy pair",
            corrupt_bundle_beside_legacy_row(),
        ),
        named("bundle preferred over legacy pair", bundle_preferred_row()),
        named("missing generation", missing_generation_row()),
    ]
}

fn malformed_bundle_row() -> Row {
    let dir = tempdir()?;
    write(dir.path(), BUNDLE, MALFORMED_PEM)?;
    expect_invalid(&api_owner(dir.path()))
}

fn keyless_bundle_row() -> Row {
    let dir = tempdir()?;
    let generation = current_leaf()?;
    write(dir.path(), BUNDLE, &generation.cert_pem)?;
    expect_invalid(&api_owner(dir.path()))
}

fn certless_bundle_row() -> Row {
    let dir = tempdir()?;
    let generation = current_leaf()?;
    write(dir.path(), BUNDLE, &generation.key_pem)?;
    expect_invalid(&api_owner(dir.path()))
}

/// A corrupt preferred bundle is a cache error, never permission to fall back
/// to the older pair, and the reader leaves every file as it found it.
fn corrupt_bundle_beside_legacy_row() -> Row {
    let dir = tempdir()?;
    let legacy = current_leaf()?;
    write(dir.path(), BUNDLE, MALFORMED_PEM)?;
    write_legacy(dir.path(), &legacy.cert_pem, &legacy.key_pem)?;
    let refused = expect_invalid(&api_owner(dir.path()));
    all([
        refused,
        expect_eq(
            "bundle bytes after refusal",
            read_bytes(&dir.path().join(BUNDLE))?,
            MALFORMED_PEM.as_bytes().to_vec(),
        ),
        expect_eq(
            "legacy certificate after refusal",
            read_bytes(&dir.path().join(LEGACY_CERT))?,
            legacy.cert_pem.as_bytes().to_vec(),
        ),
    ])
}

fn bundle_preferred_row() -> Row {
    let dir = tempdir()?;
    let current = current_leaf()?;
    let legacy = current_leaf()?;
    write_bundle(dir.path(), &current.cert_pem, &current.key_pem)?;
    write_legacy(dir.path(), &legacy.cert_pem, &legacy.key_pem)?;
    expect_admitted(&api_owner(dir.path()), &current)
}

fn missing_generation_row() -> Row {
    let dir = tempdir()?;
    match api_owner(dir.path()).load_cached_cert() {
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err("an empty cache produced a generation".to_owned()),
        Err(error) => Err(format!("an empty cache was refused with {error:?}")),
    }
}

// --- legacy import -------------------------------------------------------

fn legacy_rows() -> Vec<Row> {
    vec![
        named("valid pair migrates", legacy_migration_row()),
        named(
            "pair with a mismatched key",
            legacy_refused_row(|| {
                let certificate = current_leaf()?;
                let other = current_leaf()?;
                Ok((certificate.cert_pem, other.key_pem))
            }),
        ),
        named(
            "expired pair",
            legacy_refused_row(|| {
                let expired = leaf(&[API], -90 * DAY, -HOUR)?;
                Ok((expired.cert_pem, expired.key_pem))
            }),
        ),
        named(
            "pair not covering the domain",
            legacy_refused_row(|| {
                let uncovered = leaf(&["other.example.com"], -DAY, 60 * DAY)?;
                Ok((uncovered.cert_pem, uncovered.key_pem))
            }),
        ),
        named("pair renewal reads the leaf", legacy_renewal_row()),
    ]
}

/// A valid legacy pair is validated, published as a private bundle holding the
/// same generation, and served from that bundle once the pair is gone.
fn legacy_migration_row() -> Row {
    let dir = tempdir()?;
    let legacy = current_leaf()?;
    write_legacy(dir.path(), &legacy.cert_pem, &legacy.key_pem)?;
    let acme = api_owner(dir.path());
    expect_admitted(&acme, &legacy)?;

    let bundle = dir.path().join(BUNDLE);
    expect("the pair must be migrated to a bundle", bundle.exists())?;
    // The privacy contract is Unix-only, as the publisher's is.
    #[cfg(unix)]
    let private = expect_private(&bundle);
    std::fs::remove_file(dir.path().join(LEGACY_CERT))
        .map_err(|error| format!("remove legacy certificate: {error}"))?;
    std::fs::remove_file(dir.path().join(LEGACY_KEY))
        .map_err(|error| format!("remove legacy key: {error}"))?;
    let served =
        expect_admitted(&acme, &legacy).map_err(|reason| format!("migrated bundle: {reason}"));
    #[cfg(unix)]
    let served = all([private, served]);
    served
}

/// An invalid legacy pair is refused and never published as a bundle.
fn legacy_refused_row(pair: fn() -> Result<CertificatePair, String>) -> Row {
    let dir = tempdir()?;
    let (cert_pem, key_pem) = pair()?;
    write_legacy(dir.path(), &cert_pem, &key_pem)?;
    let refused = expect_invalid(&api_owner(dir.path()));
    all([
        refused,
        expect(
            "a refused pair must not be migrated",
            !dir.path().join(BUNDLE).exists(),
        ),
    ])
}

fn legacy_renewal_row() -> Row {
    let dir = tempdir()?;
    let legacy = current_leaf()?;
    write_legacy(dir.path(), &legacy.cert_pem, &legacy.key_pem)?;
    write_sidecar(dir.path(), 5 * DAY)?;
    expect_eq("renewal due", api_owner(dir.path()).needs_renewal(), false)
}

// --- cached generation reuse ---------------------------------------------

/// Publish a bundle whose [`CACHED`] leaf expires `days` days from now. The
/// leaf's `notAfter` is the only expiry the cache reads.
fn seed_expiring(dir: &Path, days: u32) -> Row {
    seed_cache(dir, &generation_expiring(&[CACHED], days)?)
}

#[test]
fn cert_cached_to_disk() {
    let cache_dir = TempDir::new().expect("temp dir");
    seed_expiring(cache_dir.path(), 60).expect("seed the cache");

    let config = owner(cache_dir.path(), &[CACHED]);
    let cert = config.load_cached_cert().expect("load cached cert");
    assert!(
        cert.is_some(),
        "cached cert should load without new ACME order"
    );

    let config = owner(cache_dir.path(), &[CACHED]);
    let cert = config.load_cached_cert().expect("load cached cert again");
    assert!(cert.is_some(), "cert still loadable from cache");
}

#[test]
fn renewal_triggered_before_expiry() {
    let cache_near = TempDir::new().expect("temp dir");
    seed_expiring(cache_near.path(), 15).expect("seed the near cache");
    let config_near = owner(cache_near.path(), &[CACHED]);
    assert!(
        config_near.needs_renewal(),
        "cert expiring in 15 days should need renewal"
    );

    let cache_far = TempDir::new().expect("temp dir");
    seed_expiring(cache_far.path(), 60).expect("seed the far cache");
    let config_far = owner(cache_far.path(), &[CACHED]);
    assert!(
        !config_far.needs_renewal(),
        "cert expiring in 60 days should not need renewal"
    );
}

#[cfg(unix)]
#[test]
fn credentials_are_atomically_replaced_with_private_permissions() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let cache_dir = TempDir::new().expect("temp dir");
    let credentials_path = cache_dir.path().join("account.json");
    std::fs::write(&credentials_path, b"old credentials").expect("seed credentials");
    std::fs::set_permissions(&credentials_path, std::fs::Permissions::from_mode(0o644))
        .expect("set permissive mode");
    let original_inode = std::fs::metadata(&credentials_path)
        .expect("original metadata")
        .ino();

    camber::__private::write_dns01_credentials(cache_dir.path(), b"new credentials")
        .expect("replace credentials");

    let metadata = std::fs::metadata(&credentials_path).expect("replacement metadata");
    assert_ne!(
        metadata.ino(),
        original_inode,
        "replacement must use rename"
    );
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::read(&credentials_path).expect("read replaced credentials"),
        b"new credentials"
    );
}

const BARE_NAME_TEST: &str =
    "dns_certificate_cache::credentials_named_by_a_bare_file_name_are_published";
const BARE_NAME_MODE: &str = "dns01-bare-file-name-publication";
const BARE_NAME_MARKER: &str = "dns01-bare-file-name-published";

/// The real-time bound on the whole private child. It bounds fixture failure
/// only.
const BARE_NAME_CHILD_BOUND: std::time::Duration = std::time::Duration::from_secs(60);

/// A path that is a bare file name publishes into the current directory: the
/// directory sync after the rename opens that directory, not an empty path.
///
/// Runs in a private child, because the current directory is the process's
/// one global working directory.
#[test]
fn credentials_named_by_a_bare_file_name_are_published() {
    crate::process::run_in_child(
        BARE_NAME_TEST,
        BARE_NAME_MODE,
        BARE_NAME_MARKER,
        BARE_NAME_CHILD_BOUND,
        || {
            let working = tempdir().expect("working directory");
            std::env::set_current_dir(working.path()).expect("enter the working directory");

            camber::__private::write_dns01_credentials(Path::new(""), b"credentials")
                .expect("publish credentials named by a bare file name");

            assert_eq!(
                std::fs::read(working.path().join("account.json")).expect("read credentials"),
                b"credentials"
            );
        },
    );
}

#[test]
fn credentials_writer_creates_cache_and_types_write_failures() {
    let root = TempDir::new().expect("cache root");
    let cache = root.path().join("new/cache");
    camber::__private::write_dns01_credentials(&cache, b"credentials")
        .expect("create the cache directory");
    assert_eq!(
        std::fs::read(cache.join("account.json")).expect("account bytes"),
        b"credentials"
    );
    let obstruction = root.path().join("not-a-directory");
    std::fs::write(&obstruction, b"keep").expect("seed obstruction");
    let error = camber::__private::write_dns01_credentials(&obstruction, b"credentials")
        .expect_err("a file cannot hold the cache");
    let RuntimeError::Integration(error) = error else {
        panic!("untyped cache error: {error:?}")
    };
    assert_eq!(error.operation(), IntegrationOperation::CacheWrite);
    assert_eq!(std::fs::read(obstruction).expect("original file"), b"keep");
}
