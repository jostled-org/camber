#![cfg(feature = "acme")]

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
