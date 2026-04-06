use camber::config::{TlsConfig, load_config};
use std::io::Write;
use tempfile::NamedTempFile;

fn write_toml(content: &str) -> NamedTempFile {
    let mut f = NamedTempFile::new().expect("temp file");
    f.write_all(content.as_bytes()).expect("write");
    f
}

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

#[test]
fn load_config_parses_toml_file() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        name: Box<str>,
        port: u16,
    }

    let f = write_toml(
        r#"
name = "test-app"
port = 8080
"#,
    );

    let config: TestConfig = load_config(f.path()).unwrap();
    assert_eq!(&*config.name, "test-app");
    assert_eq!(config.port, 8080);
}

#[test]
fn load_config_returns_error_on_missing_file() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        _name: Box<str>,
    }

    let result = load_config::<TestConfig>(std::path::Path::new("/nonexistent/config.toml"));
    assert!(result.is_err());
}

#[test]
fn load_config_returns_error_on_invalid_toml() {
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    struct TestConfig {
        _name: Box<str>,
    }

    let f = write_toml("this is not valid = = = toml [[[");

    let result = load_config::<TestConfig>(f.path());
    assert!(result.is_err());
}
