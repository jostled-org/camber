#![cfg(feature = "acme")]

#[cfg(feature = "dns01")]
use crate::acme_validation_rows;
#[cfg(feature = "dns01")]
use crate::integration_rows::run_rows_under;
use camber::acme::AcmeConfig;

#[test]
fn acme_config_default_cache_dir_uses_tool_name() {
    let config = AcmeConfig::new("camber", ["example.com"]);
    let cache_dir = format!("{}", config.cache_path().display());
    assert!(
        cache_dir.ends_with(".config/camber/certs"),
        "expected cache_dir to end with .config/camber/certs, got: {cache_dir}"
    );
}

#[test]
fn acme_config_custom_cache_dir() {
    let tmp = tempfile::tempdir().expect("failed to create tempdir");
    let config = AcmeConfig::new("camber", ["example.com"]).cache_dir(tmp.path());
    assert_eq!(config.cache_path(), tmp.path());
}

#[test]
fn acme_config_builds_server_config() {
    let tmp = tempfile::tempdir().expect("failed to create tempdir");

    let config = AcmeConfig::new("camber", ["example.com"])
        .staging(true)
        .cache_dir(tmp.path());

    let result = config.build();
    assert!(result.is_ok(), "build() failed: {result:?}");

    let (server_config, _state) = result.expect("already checked");
    assert!(
        !server_config.alpn_protocols.is_empty(),
        "expected ALPN protocols to be configured"
    );
}

/// 7.T1: every ACME name set and TLS mode is validated before any effect.
///
/// TLS-ALPN-01 is refused at `AcmeConfig::build`, before its cache exists. DNS-01
/// is refused at `provision_cert` against a cache seeded with malformed
/// account credentials: a configuration refusal there proves validation ran
/// before the credential load, and a valid set proves it passed through to
/// that load. The provider records every call, so a refused set shows zero.
/// `TlsConfig` rows cover provider names, mode-unused fields, and unknown
/// input fields. Every row runs; the failures are reported together.
#[cfg(feature = "dns01")]
#[test]
fn acme_names_and_tls_modes_reject_invalid_sets_before_effects() {
    run_rows_under(
        "M9 invalid TLS configuration must precede effects",
        &[
            ("TLS-ALPN-01 names", acme_validation_rows::tls_alpn01_rows),
            ("DNS-01 names", acme_validation_rows::dns01_rows),
            ("TLS modes", acme_validation_rows::tls_mode_rows),
            ("TLS input fields", acme_validation_rows::tls_field_rows),
        ],
    );
}
