use camber::config::TlsConfig;

#[test]
fn tls_config_validates_auto_requires_email() {
    let tls = TlsConfig {
        auto: Some(true),
        email: None,
        staging: None,
        cert: None,
        key: None,
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    };

    let err = tls.validate().unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("email"), "error should mention email: {msg}");
}

#[test]
fn tls_config_validates_auto_rejects_cert_key() {
    let tls = TlsConfig {
        auto: Some(true),
        email: Some("admin@example.com".into()),
        staging: None,
        cert: Some("/etc/cert.pem".into()),
        key: Some("/etc/key.pem".into()),
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    };

    let err = tls.validate().unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("mutually exclusive"),
        "error should mention mutually exclusive: {msg}"
    );
}

#[test]
fn tls_config_validates_partial_cert_key() {
    let tls = TlsConfig {
        auto: None,
        email: None,
        staging: None,
        cert: Some("/etc/cert.pem".into()),
        key: None,
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    };

    let err = tls.validate().unwrap_err();
    assert!(err.to_string().contains("both cert and key"));
}

#[test]
fn tls_config_validates_valid_manual() {
    let tls = TlsConfig {
        auto: None,
        email: None,
        staging: None,
        cert: Some("/etc/cert.pem".into()),
        key: Some("/etc/key.pem".into()),
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    };

    assert!(tls.validate().is_ok());
}

#[test]
fn tls_config_validates_valid_auto() {
    let tls = TlsConfig {
        auto: Some(true),
        email: Some("admin@example.com".into()),
        staging: None,
        cert: None,
        key: None,
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    };

    assert!(tls.validate().is_ok());
}
