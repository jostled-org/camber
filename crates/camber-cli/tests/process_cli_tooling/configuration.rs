use camber_cli::config::Config;
use std::io::Write;
use tempfile::NamedTempFile;

use crate::support::FixtureError;

fn write_config(toml: &str) -> Result<NamedTempFile, FixtureError> {
    let mut file = NamedTempFile::new()?;
    file.write_all(toml.as_bytes())?;
    Ok(file)
}

fn config_error(path: &std::path::Path) -> Result<String, FixtureError> {
    match Config::load(path) {
        Ok(_) => Err(FixtureError::new("invalid configuration was accepted")),
        Err(error) => Ok(error),
    }
}

#[test]
fn parse_minimal_config() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
listen = ":8443"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    assert_eq!(config.listen(), ":8443");
    assert_eq!(config.sites().len(), 1);
    assert_eq!(config.sites()[0].host(), "app.example.com");
    assert_eq!(config.sites()[0].proxy(), Some("http://localhost:3000"));
    assert_eq!(config.sites()[0].root(), None);
    assert!(config.tls().is_none());
    Ok(())
}

#[test]
fn parse_full_config() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
listen = ":443"
[tls]
cert = "/etc/camber/cert.pem"
key = "/etc/camber/key.pem"
[[site]]
host = "blog.example.com"
proxy = "http://localhost:3000"
[[site]]
host = "docs.example.com"
root = "/var/www/docs"
[[site]]
host = "app.example.com"
proxy = "http://localhost:8080"
root = "/var/www/static"
"#,
    )?;
    let config = Config::load(file.path())?;
    assert_eq!(config.listen(), ":443");
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("manual TLS configuration was absent"))?;
    assert_eq!(tls.cert(), Some("/etc/camber/cert.pem"));
    assert_eq!(tls.key(), Some("/etc/camber/key.pem"));
    assert_eq!(config.sites().len(), 3);
    assert_eq!(config.sites()[0].host(), "blog.example.com");
    assert_eq!(config.sites()[0].proxy(), Some("http://localhost:3000"));
    assert_eq!(config.sites()[0].root(), None);
    assert_eq!(config.sites()[1].host(), "docs.example.com");
    assert_eq!(config.sites()[1].proxy(), None);
    assert_eq!(config.sites()[1].root(), Some("/var/www/docs"));
    assert_eq!(config.sites()[2].host(), "app.example.com");
    assert_eq!(config.sites()[2].proxy(), Some("http://localhost:8080"));
    assert_eq!(config.sites()[2].root(), Some("/var/www/static"));
    Ok(())
}

#[test]
fn parse_config_rejects_site_without_proxy_or_root() -> Result<(), FixtureError> {
    let file = write_config(
        r#"[[site]]
host = "empty.example.com"
"#,
    )?;
    let error = config_error(file.path())?;
    assert!(
        error.contains("empty.example.com"),
        "error should name the offending host: {error}"
    );
    Ok(())
}

#[test]
fn parse_config_default_listen_address() -> Result<(), FixtureError> {
    let file = write_config(
        r#"[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    assert_eq!(Config::load(file.path())?.listen(), "0.0.0.0:8080");
    Ok(())
}

#[test]
fn parse_auto_tls_config() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
listen = ":443"
[tls]
auto = true
email = "admin@example.com"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("automatic TLS configuration was absent"))?;
    assert!(tls.auto());
    assert_eq!(tls.email(), Some("admin@example.com"));
    assert!(tls.cert().is_none());
    assert!(tls.key().is_none());
    Ok(())
}

#[test]
fn auto_tls_rejects_missing_email() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let error = config_error(file.path())?;
    assert!(
        error.contains("email"),
        "error should mention email: {error}"
    );
    Ok(())
}

#[test]
fn auto_tls_rejects_combined_with_manual_cert() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
cert = "/etc/camber/cert.pem"
key = "/etc/camber/key.pem"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let error = config_error(file.path())?;
    assert!(
        error.contains("mutually exclusive") || error.contains("auto") && error.contains("cert"),
        "error should mention conflict: {error}"
    );
    Ok(())
}

#[test]
fn auto_tls_collects_domains_from_sites() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
[[site]]
host = "api.example.com"
proxy = "http://localhost:8080"
[[site]]
host = "docs.example.com"
root = "/var/www/docs"
"#,
    )?;
    let config = Config::load(file.path())?;
    let domains = config.auto_tls_domains();
    assert_eq!(domains.len(), 3);
    assert!(domains.contains(&"example.com"));
    assert!(domains.contains(&"api.example.com"));
    assert!(domains.contains(&"docs.example.com"));
    Ok(())
}

#[test]
fn config_parses_health_check_fields() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
health_check = "/health"
health_interval = 5
[[site]]
host = "static.example.com"
root = "/var/www/html"
"#,
    )?;
    let config = Config::load(file.path())?;
    assert_eq!(config.sites().len(), 2);
    assert_eq!(config.sites()[0].health_check(), Some("/health"));
    assert_eq!(config.sites()[0].health_interval(), Some(5));
    assert_eq!(config.sites()[1].health_check(), None);
    assert_eq!(config.sites()[1].health_interval(), None);
    Ok(())
}

#[test]
fn config_parses_without_health_check() -> Result<(), FixtureError> {
    let file = write_config(
        r#"[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    assert_eq!(config.sites()[0].health_check(), None);
    assert_eq!(config.sites()[0].health_interval(), None);
    Ok(())
}

#[test]
fn config_parses_dns_provider_with_env_token() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_env = "CF_TOKEN"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("DNS TLS configuration was absent"))?;
    assert!(tls.auto());
    assert_eq!(tls.dns_provider(), Some("cloudflare"));
    assert_eq!(tls.dns_api_token_env(), Some("CF_TOKEN"));
    assert!(tls.dns_api_token_file().is_none());
    Ok(())
}

#[test]
fn config_parses_dns_provider_with_file_token() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_file = "/etc/camber/cf.token"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("DNS TLS configuration was absent"))?;
    assert!(tls.auto());
    assert_eq!(tls.dns_provider(), Some("cloudflare"));
    assert!(tls.dns_api_token_env().is_none());
    assert_eq!(tls.dns_api_token_file(), Some("/etc/camber/cf.token"));
    Ok(())
}

#[test]
fn auto_tls_without_dns_provider_is_valid() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("automatic TLS configuration was absent"))?;
    assert!(tls.auto());
    assert!(tls.dns_provider().is_none());
    Ok(())
}

#[test]
fn config_rejects_dns_provider_without_token() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let error = config_error(file.path())?;
    assert!(
        error.contains("token"),
        "error should mention token: {error}"
    );
    Ok(())
}

#[test]
fn config_rejects_both_token_env_and_file() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
dns_provider = "cloudflare"
dns_api_token_env = "CF_TOKEN"
dns_api_token_file = "/etc/camber/cf.token"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let error = config_error(file.path())?;
    assert!(
        error.contains("mutually exclusive")
            || error.contains("dns_api_token_env") && error.contains("dns_api_token_file"),
        "error should mention mutual exclusion: {error}"
    );
    Ok(())
}

#[test]
fn parse_config_reads_connection_limit() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
connection_limit = 100
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    assert_eq!(Config::load(file.path())?.connection_limit(), Some(100));
    Ok(())
}

#[test]
fn parse_config_connection_limit_defaults_to_none() -> Result<(), FixtureError> {
    let file = write_config(
        r#"[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    assert_eq!(Config::load(file.path())?.connection_limit(), None);
    Ok(())
}

#[test]
fn parse_config_rejects_zero_connection_limit() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
connection_limit = 0
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    assert_eq!(
        config_error(file.path())?,
        "connection_limit must be at least 1"
    );
    Ok(())
}

#[test]
fn parse_config_rejects_zero_health_interval() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
health_check = "/health"
health_interval = 0
"#,
    )?;
    assert_eq!(
        config_error(file.path())?,
        "site \"app.example.com\" health_interval must be at least 1"
    );
    Ok(())
}
