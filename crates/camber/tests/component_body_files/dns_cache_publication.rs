#![cfg(feature = "dns01")]

//! 9.T3: an interrupted certificate publication leaves one whole generation.
//!
//! `DnsCachePublicationProbe` drives the one publisher issuance uses and faults
//! one of its stages. Every row reopens the cache through the public reader,
//! `AcmeDns01::load_cached_cert`. The row asserts that the reader admits the old
//! generation or the new one, never a mix. It also asserts that the bundle stays
//! private, that no temporary file remains, and that the publisher reported
//! `CacheWrite`.

use crate::dns_cache_files::{BUNDLE, DAY, Leaf, leaf};
#[cfg(unix)]
use crate::dns_cache_files::{expect_private, make_private};
use crate::integration_rows::{Row, all, tempdir};
use camber::dns01::AcmeDns01;
use camber::runtime_test_support::{DnsCachePublicationProbe, DnsCachePublicationStage};
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, RuntimeError};
use std::path::Path;

const DOMAIN: &str = "api.example.com";

/// Each faulted stage, and whether the rename had replaced the bundle before
/// the fault.
const STAGES: [(DnsCachePublicationStage, bool); 6] = [
    (DnsCachePublicationStage::Create, false),
    (DnsCachePublicationStage::Write, false),
    (DnsCachePublicationStage::Sync, false),
    (DnsCachePublicationStage::Permissions, false),
    (DnsCachePublicationStage::Rename, false),
    (DnsCachePublicationStage::DirectorySync, true),
];

#[test]
fn dns_cache_publication_is_atomic_under_every_interruption() {
    let interrupted = STAGES.iter().flat_map(|&(stage, replaced)| {
        [
            (
                "over a prior generation",
                interrupted_over_prior(stage, replaced),
            ),
            (
                "into an empty cache",
                interrupted_into_empty(stage, replaced),
            ),
        ]
        .into_iter()
        .map(move |(case, row)| row.map_err(|reason| format!("{stage:?} {case}: {reason}")))
    });
    let invalid = invalid_generation_is_never_published()
        .map_err(|reason| format!("invalid generation: {reason}"));
    all(interrupted.chain([invalid]))
        .expect("an interrupted publication must leave one whole generation");
}

/// One current leaf for [`DOMAIN`] and its key.
fn current_leaf() -> Result<Leaf, String> {
    leaf(&[DOMAIN], -DAY, 60 * DAY)
}

fn owner(dir: &Path) -> AcmeDns01 {
    AcmeDns01::new("camber-test", [DOMAIN]).cache_dir(dir)
}

/// Seed `dir` with `generation` as a bundle, private where the privacy
/// contract holds: Unix-only, as the publisher's is.
fn seed(dir: &Path, generation: &Leaf) -> Row {
    let path = dir.join(BUNDLE);
    std::fs::write(&path, generation.bundle()).map_err(|error| format!("seed bundle: {error}"))?;
    #[cfg(unix)]
    make_private(&path)?;
    Ok(())
}

/// Fail unless the cache directory holds nothing but the bundle, if any.
fn expect_no_leftovers(dir: &Path) -> Row {
    let entries = std::fs::read_dir(dir).map_err(|error| format!("list cache: {error}"))?;
    let stray = entries
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| format!("read cache entry: {error}"))
        })
        .filter(|name| name.as_deref() != Ok(BUNDLE))
        .collect::<Result<Box<[String]>, String>>()?;
    match stray.is_empty() {
        true => Ok(()),
        false => Err(format!("temporary files remain: {stray:?}")),
    }
}

/// The typed refusal `outcome` carries, or why it carries none.
fn refusal_of(
    outcome: Result<(), RuntimeError>,
) -> Result<(IntegrationKind, IntegrationOperation, IntegrationFailure), String> {
    match outcome {
        Ok(()) => Err("the publication reported success".to_owned()),
        Err(RuntimeError::Integration(error)) => {
            Ok((error.kind(), error.operation(), error.failure()))
        }
        Err(other) => Err(format!("{other:?} is not a typed integration refusal")),
    }
}

/// Fail unless `outcome` is the typed `operation` refusal from the DNS-01 owner.
fn expect_refused(outcome: Result<(), RuntimeError>, operation: IntegrationOperation) -> Row {
    let (kind, found, _) = refusal_of(outcome)?;
    match (kind, found == operation) {
        (IntegrationKind::Dns01, true) => Ok(()),
        _ => Err(format!(
            "refused as {kind:?} {found:?}, expected {operation:?}"
        )),
    }
}

/// The leaf the public reader admits from `dir`, `None` when it finds no
/// generation, or why it refuses the cache.
fn admitted_leaf(dir: &Path) -> Result<Option<Box<[u8]>>, String> {
    let key = owner(dir)
        .load_cached_cert()
        .map_err(|error| format!("the reader refused the cache with {error:?}"))?;
    key.map(|key| {
        key.cert
            .first()
            .map(|der| der.as_ref().into())
            .ok_or_else(|| "the reader admitted an empty chain".to_owned())
    })
    .transpose()
}

/// Fail unless the public reader admits exactly `expected`'s leaf.
fn expect_generation(dir: &Path, expected: &Leaf) -> Row {
    match admitted_leaf(dir)? {
        Some(leaf) if leaf == expected.der => Ok(()),
        Some(_) => Err("the reader admitted a different generation".to_owned()),
        None => Err("the reader found no generation".to_owned()),
    }
}

/// Fail unless the public reader admits exactly `expected`'s leaf from a
/// bundle only its owner can read, where the privacy contract holds.
fn expect_whole(dir: &Path, expected: &Leaf) -> Row {
    let whole = expect_generation(dir, expected);
    // The privacy contract is Unix-only, as the publisher's is.
    #[cfg(unix)]
    let whole = all([whole, expect_private(&dir.join(BUNDLE))]);
    whole
}

fn expect_absent(dir: &Path) -> Row {
    match admitted_leaf(dir)? {
        None => Ok(()),
        Some(_) => Err("an interrupted first publication left a generation".to_owned()),
    }
}

/// A fault at `stage` over a prior generation leaves the old generation, or
/// the new one once the rename has happened, and always reports `CacheWrite`.
fn interrupted_over_prior(stage: DnsCachePublicationStage, replaced: bool) -> Row {
    let dir = tempdir()?;
    let old = current_leaf()?;
    let new = current_leaf()?;
    seed(dir.path(), &old)?;
    let outcome =
        DnsCachePublicationProbe::fail_at(&owner(dir.path()), &new.cert_pem, &new.key_pem, stage);
    let survivor = match replaced {
        true => &new,
        false => &old,
    };
    all([
        expect_refused(outcome, IntegrationOperation::CacheWrite),
        expect_whole(dir.path(), survivor),
        expect_no_leftovers(dir.path()),
    ])
}

/// A fault at `stage` into an empty cache leaves no generation, or the whole
/// new one once the rename has happened.
fn interrupted_into_empty(stage: DnsCachePublicationStage, replaced: bool) -> Row {
    let dir = tempdir()?;
    let new = current_leaf()?;
    let outcome =
        DnsCachePublicationProbe::fail_at(&owner(dir.path()), &new.cert_pem, &new.key_pem, stage);
    let survivor = match replaced {
        true => expect_whole(dir.path(), &new),
        false => expect_absent(dir.path()),
    };
    all([
        expect_refused(outcome, IntegrationOperation::CacheWrite),
        survivor,
        expect_no_leftovers(dir.path()),
    ])
}

/// The probe validates as issuance does: a generation whose key belongs to
/// another leaf is refused before any stage runs, and the old one stays.
fn invalid_generation_is_never_published() -> Row {
    let dir = tempdir()?;
    let old = current_leaf()?;
    let certificate = current_leaf()?;
    let other = current_leaf()?;
    seed(dir.path(), &old)?;
    let outcome = DnsCachePublicationProbe::fail_at(
        &owner(dir.path()),
        &certificate.cert_pem,
        &other.key_pem,
        DnsCachePublicationStage::DirectorySync,
    );
    let refused = match refusal_of(outcome)? {
        (
            IntegrationKind::Dns01,
            IntegrationOperation::Provision,
            IntegrationFailure::InvalidCertificate,
        ) => Ok(()),
        other => Err(format!("refused as {other:?}")),
    };
    all([
        refused,
        expect_generation(dir.path(), &old),
        expect_no_leftovers(dir.path()),
    ])
}
