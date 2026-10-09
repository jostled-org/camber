use camber::config::{AcmeSettings, TlsMode};
use camber::secret::SecretRef;
use camber_cli::config::Config;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::NamedTempFile;

use crate::support::process::camber_bin;
use crate::support::{
    CONFIG_REFUSAL_BOUND, FixtureError, failed_checks, is_fifo, make_fifo,
    run_command_with_timeout, run_command_within, serve_command,
};

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
    let docs = tempfile::tempdir()?;
    let assets = tempfile::tempdir()?;
    let docs_root = docs.path().display().to_string();
    let assets_root = assets.path().display().to_string();
    let file = write_config(&format!(
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
root = "{docs_root}"
[[site]]
host = "app.example.com"
proxy = "http://localhost:8080"
root = "{assets_root}"
"#
    ))?;
    let config = Config::load(file.path())?;
    assert_eq!(config.listen(), ":443");
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("manual TLS configuration was absent"))?;
    let TlsMode::Manual { cert, key } = tls else {
        return Err(FixtureError::new(format!(
            "expected manual TLS, got {tls:?}"
        )));
    };
    assert_eq!(&**cert, "/etc/camber/cert.pem");
    assert_eq!(&**key, "/etc/camber/key.pem");
    assert_eq!(config.sites().len(), 3);
    assert_eq!(config.sites()[0].host(), "blog.example.com");
    assert_eq!(config.sites()[0].proxy(), Some("http://localhost:3000"));
    assert_eq!(config.sites()[0].root(), None);
    assert_eq!(config.sites()[1].host(), "docs.example.com");
    assert_eq!(config.sites()[1].proxy(), None);
    assert_eq!(config.sites()[1].root(), Some(docs_root.as_str()));
    assert_eq!(config.sites()[2].host(), "app.example.com");
    assert_eq!(config.sites()[2].proxy(), Some("http://localhost:8080"));
    assert_eq!(config.sites()[2].root(), Some(assets_root.as_str()));
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
    let TlsMode::TlsAlpn(acme) = tls else {
        return Err(FixtureError::new(format!(
            "expected TLS-ALPN-01, got {tls:?}"
        )));
    };
    assert_eq!(
        acme,
        &AcmeSettings {
            email: "admin@example.com".into(),
            staging: false,
            cache_dir: None,
        }
    );
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
    let docs = tempfile::tempdir()?;
    let file = write_config(&format!(
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
root = "{}"
"#,
        docs.path().display()
    ))?;
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
    let html = tempfile::tempdir()?;
    let file = write_config(&format!(
        r#"
[[site]]
host = "app.example.com"
proxy = "http://localhost:3000"
health_check = "/health"
health_interval = 5
[[site]]
host = "static.example.com"
root = "{}"
"#,
        html.path().display()
    ))?;
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
    let TlsMode::Dns01 {
        acme,
        token: SecretRef::Env(name),
    } = tls
    else {
        return Err(FixtureError::new(format!(
            "expected DNS-01 with an environment token, got {tls:?}"
        )));
    };
    assert_eq!(&*acme.email, "admin@example.com");
    assert_eq!(&**name, "CF_TOKEN");
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
    let TlsMode::Dns01 {
        acme,
        token: SecretRef::File(path),
    } = tls
    else {
        return Err(FixtureError::new(format!(
            "expected DNS-01 with a file token, got {tls:?}"
        )));
    };
    assert_eq!(&*acme.email, "admin@example.com");
    assert_eq!(&**path, "/etc/camber/cf.token");
    Ok(())
}

#[test]
fn auto_tls_without_dns_provider_is_valid() -> Result<(), FixtureError> {
    let file = write_config(
        r#"
[tls]
auto = true
email = "admin@example.com"
staging = true
cache_dir = "/var/cache/camber"
[[site]]
host = "example.com"
proxy = "http://localhost:3000"
"#,
    )?;
    let config = Config::load(file.path())?;
    let tls = config
        .tls()
        .ok_or_else(|| FixtureError::new("automatic TLS configuration was absent"))?;
    let TlsMode::TlsAlpn(acme) = tls else {
        return Err(FixtureError::new(format!(
            "expected TLS-ALPN-01, got {tls:?}"
        )));
    };
    assert_eq!(
        acme,
        &AcmeSettings {
            email: "admin@example.com".into(),
            staging: true,
            cache_dir: Some("/var/cache/camber".into()),
        }
    );
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

/// 7.T2: an invalid DNS provider is refused before the token is read.
///
/// The token file is a FIFO with no writer, so any read blocks in `open`
/// until the bound kills the child. A CLI that validates first exits on its
/// own, names `dns_provider`, never names the secret, never logs a listener,
/// and never creates its ACME cache.
#[test]
fn invalid_dns_provider_never_loads_a_secret() -> Result<(), FixtureError> {
    let failures: Box<[String]> = ["route53", "", "Cloudflare"]
        .into_iter()
        .filter_map(|provider| {
            invalid_provider_row(provider)
                .err()
                .map(|reason| format!("dns_provider {provider:?}: {reason}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "M9 invalid TLS configuration must precede effects: {} rows failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}

fn invalid_provider_row(provider: &str) -> Result<(), FixtureError> {
    let root = tempfile::tempdir()?;
    let observed = observe_provider_refusal(provider, root.path());
    FixtureError::with_cleanup(observed, root.close().map_err(FixtureError::from))
}

fn observe_provider_refusal(provider: &str, root: &std::path::Path) -> Result<(), FixtureError> {
    let secret = root.join("token.fifo");
    make_fifo(&secret)?;
    let cache = root.join("acme-cache");
    let config = root.join("camber.toml");
    std::fs::write(
        &config,
        format!(
            "listen = \"127.0.0.1:0\"\n\
             [tls]\n\
             auto = true\n\
             email = \"admin@example.com\"\n\
             staging = true\n\
             cache_dir = \"{cache}\"\n\
             dns_provider = \"{provider}\"\n\
             dns_api_token_file = \"{secret}\"\n\
             [[site]]\n\
             host = \"example.com\"\n\
             proxy = \"http://127.0.0.1:9\"\n",
            cache = cache.display(),
            secret = secret.display(),
        ),
    )?;

    let output = run_command_within(serve_command(&config), CONFIG_REFUSAL_BOUND)?.ok_or_else(
        || {
            FixtureError::new(format!(
                "the CLI did not exit within {CONFIG_REFUSAL_BOUND:?}: it opened the secret or served"
            ))
        },
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let secret_path = secret.display().to_string();

    let checks = [
        (!output.status.success(), "the CLI exited successfully"),
        (
            stderr.contains("dns_provider"),
            "the refusal does not name dns_provider",
        ),
        (
            !stdout.contains(&secret_path) && !stderr.contains(&secret_path),
            "the output names the secret source",
        ),
        (!output.announced_listener(), "the CLI reported a listener"),
        (!cache.exists(), "the ACME cache was created"),
        (is_fifo(&secret)?, "the secret FIFO was replaced"),
    ];
    match failed_checks(&checks) {
        None => Ok(()),
        Some(failed) => Err(FixtureError::new(format!("{failed}; stderr: {stderr}"))),
    }
}

/// What `Config::load` must answer for one 8.T1 row.
enum SiteExpectation {
    Refused,
    /// Accepted, and the check holds on the loaded configuration.
    Accepted(fn(&Config) -> Result<(), String>),
}

struct SiteRow {
    name: &'static str,
    toml: Box<str>,
    expectation: SiteExpectation,
}

/// Directories the root rows point at, owned for the whole table.
struct SiteRoots {
    temp: tempfile::TempDir,
    readable: Box<str>,
    unreadable: Box<std::path::Path>,
    file: Box<str>,
    missing: Box<str>,
}

impl SiteRoots {
    fn create() -> Result<Self, FixtureError> {
        let temp = tempfile::tempdir()?;
        let readable = temp.path().join("site-root");
        std::fs::create_dir(&readable)?;
        std::fs::write(readable.join("index.html"), "<h1>root</h1>")?;
        let unreadable = temp.path().join("unreadable-root");
        std::fs::create_dir(&unreadable)?;
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000))?;
        let file = temp.path().join("root-file");
        std::fs::write(&file, "not a directory")?;
        Ok(Self {
            readable: readable.display().to_string().into_boxed_str(),
            unreadable: unreadable.into_boxed_path(),
            file: file.display().to_string().into_boxed_str(),
            missing: temp
                .path()
                .join("missing-root")
                .display()
                .to_string()
                .into_boxed_str(),
            temp,
        })
    }

    /// The unreadable row proves nothing when this process can still list
    /// the directory, so a privileged run fails the fixture instead of
    /// letting the row pass without its claim.
    fn unreadable_is_unreadable(&self) -> Result<(), FixtureError> {
        match std::fs::read_dir(&self.unreadable) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(()),
            Err(error) => Err(error.into()),
            Ok(_) => Err(FixtureError::new(format!(
                "{} stayed listable at mode 000; the unreadable-root row cannot be proven as this user",
                self.unreadable.display()
            ))),
        }
    }

    fn finish(self) -> Result<(), FixtureError> {
        std::fs::set_permissions(&self.unreadable, std::fs::Permissions::from_mode(0o755))?;
        self.temp.close()?;
        Ok(())
    }
}

fn site(host: &str, body: &str) -> String {
    format!("[[site]]\nhost = \"{host}\"\n{body}\n")
}

fn proxy_site(host: &str) -> String {
    site(host, "proxy = \"http://127.0.0.1:9\"")
}

fn proxied_to(proxy: &str) -> String {
    site("app.test", &format!("proxy = \"{proxy}\""))
}

fn refused(name: &'static str, toml: String) -> SiteRow {
    SiteRow {
        name,
        toml: toml.into_boxed_str(),
        expectation: SiteExpectation::Refused,
    }
}

fn accepted(name: &'static str, toml: String, check: fn(&Config) -> Result<(), String>) -> SiteRow {
    SiteRow {
        name,
        toml: toml.into_boxed_str(),
        expectation: SiteExpectation::Accepted(check),
    }
}

fn loads(_: &Config) -> Result<(), String> {
    Ok(())
}

fn authority_rows() -> Vec<SiteRow> {
    let invalid_hosts = [
        ("empty host", ""),
        ("wildcard host", "*.example.com"),
        ("embedded wildcard host", "app.*.example.com"),
        ("host with path", "example.com/app"),
        ("host with userinfo", "user@example.com"),
        ("host with scheme", "http://example.com"),
        ("host with whitespace", "exa mple.com"),
        ("host with hyphen-edged label", "-app.example.com"),
        ("host with empty label", "app..example.com"),
        ("host with numeric non-IP name", "999.1.1.1"),
        ("unbracketed IPv6 host", "::1"),
        ("unclosed IPv6 host", "[::1"),
        ("bracketed non-IPv6 host", "[example.com]"),
        ("port zero", "example.com:0"),
        ("port above 65535", "example.com:65536"),
        ("empty port", "example.com:"),
        ("non-numeric port", "example.com:http"),
        ("IPv6 port above 65535", "[::1]:70000"),
    ];
    let mut rows: Vec<SiteRow> = invalid_hosts
        .into_iter()
        .map(|(name, host)| refused(name, proxy_site(host)))
        .collect();
    let duplicates = [
        ("duplicate host", "example.com", "example.com"),
        (
            "duplicate host after case folding",
            "example.com",
            "EXAMPLE.com",
        ),
        (
            "duplicate host after trailing dot",
            "example.com",
            "example.com.",
        ),
        (
            "duplicate IPv4 authority",
            "127.0.0.1:8080",
            "127.0.0.1:8080",
        ),
        (
            "duplicate IPv6 authority after normalization",
            "[::1]",
            "[0:0:0:0:0:0:0:1]",
        ),
    ];
    rows.extend(duplicates.into_iter().map(|(name, first, second)| {
        refused(name, format!("{}{}", proxy_site(first), proxy_site(second)))
    }));
    rows.extend([
        accepted("IPv4 authority", proxy_site("127.0.0.1"), loads),
        accepted(
            "IPv4 authority with port",
            proxy_site("127.0.0.1:8080"),
            loads,
        ),
        accepted("IPv6 authority", proxy_site("[::1]"), loads),
        accepted("IPv6 authority with port", proxy_site("[::1]:8443"), loads),
        accepted(
            "DNS authority with port",
            proxy_site("app.test:8080"),
            loads,
        ),
        refused(
            "same routing host with and without a port",
            format!("{}{}", proxy_site("app.test"), proxy_site("app.test:8080")),
        ),
    ]);
    rows
}

#[test]
fn site_ports_do_not_distinguish_routing_hosts() -> Result<(), FixtureError> {
    for (first, second) in [
        ("App.Test.:8080", "app.test:9090"),
        ("127.0.0.1:8080", "127.0.0.1:9090"),
        ("[::1]:8080", "[0:0:0:0:0:0:0:1]:9090"),
    ] {
        let file = write_config(&format!("{}{}", proxy_site(first), proxy_site(second)))?;
        let loaded = Config::load(file.path());
        file.close()?;
        let error = loaded.expect_err("sites with one routing host must be refused");
        assert!(error.contains("appears more than once"), "{error}");
    }
    Ok(())
}

fn document_rows() -> Vec<SiteRow> {
    vec![
        refused("no site table", "listen = \"127.0.0.1:0\"\n".to_owned()),
        refused("empty site array", "site = []\n".to_owned()),
        refused(
            "unknown top-level field",
            format!("listen_backlog = 16\n{}", proxy_site("app.test")),
        ),
        refused(
            "unknown site field",
            site(
                "app.test",
                "proxy = \"http://127.0.0.1:9\"\nupstream = \"x\"",
            ),
        ),
        refused(
            "misspelled site field",
            site(
                "app.test",
                "proxy = \"http://127.0.0.1:9\"\nhealth_checks = \"/health\"",
            ),
        ),
    ]
}

fn proxy_rows() -> Vec<SiteRow> {
    let invalid_proxies = [
        ("proxy without scheme", "127.0.0.1:9"),
        ("proxy with ftp scheme", "ftp://127.0.0.1:9"),
        ("proxy with ws scheme", "ws://127.0.0.1:9"),
        ("proxy without authority", "http://"),
        ("proxy with path but no authority", "http:///prefix"),
        ("proxy with credentials", "http://user:secret@127.0.0.1:9"),
        ("proxy with username", "http://user@127.0.0.1:9"),
        ("proxy with query", "http://127.0.0.1:9/?token=secret"),
        ("proxy with empty query", "http://127.0.0.1:9/?"),
        ("proxy with fragment", "http://127.0.0.1:9/#frag"),
        ("proxy with invalid port", "http://127.0.0.1:65536"),
        ("empty proxy", ""),
    ];
    let mut rows: Vec<SiteRow> = invalid_proxies
        .into_iter()
        .map(|(name, proxy)| refused(name, proxied_to(proxy)))
        .collect();
    rows.extend([
        accepted(
            "proxy with path prefix",
            proxied_to("http://127.0.0.1:9/api/v1"),
            |config| expect_proxy(config, "http://127.0.0.1:9/api/v1"),
        ),
        accepted("https proxy", proxied_to("https://backend.test"), loads),
        accepted("IPv6 proxy", proxied_to("http://[::1]:9"), loads),
    ]);
    rows
}

fn expect_proxy(config: &Config, proxy: &str) -> Result<(), String> {
    match config.sites().first().and_then(|site| site.proxy()) {
        Some(found) if found == proxy => Ok(()),
        found => Err(format!("proxy was {found:?}, expected {proxy:?}")),
    }
}

fn health_rows(roots: &SiteRoots) -> Vec<SiteRow> {
    let readable = &roots.readable;
    let invalid_health = [
        ("relative health path", "health"),
        ("empty health path", ""),
        ("health path with authority", "//evil.test/health"),
        ("health URL", "http://127.0.0.1:9/health"),
        ("health path with fragment", "/health#ready"),
    ];
    let mut rows: Vec<SiteRow> = invalid_health
        .into_iter()
        .map(|(name, path)| {
            refused(
                name,
                site(
                    "app.test",
                    &format!("proxy = \"http://127.0.0.1:9\"\nhealth_check = \"{path}\""),
                ),
            )
        })
        .collect();
    rows.extend([
        refused(
            "health check without proxy",
            site(
                "static.test",
                &format!("root = \"{readable}\"\nhealth_check = \"/health\""),
            ),
        ),
        refused(
            "health interval without health check",
            site(
                "app.test",
                "proxy = \"http://127.0.0.1:9\"\nhealth_interval = 5",
            ),
        ),
        refused(
            "zero health interval",
            site(
                "app.test",
                "proxy = \"http://127.0.0.1:9\"\nhealth_check = \"/health\"\nhealth_interval = 0",
            ),
        ),
        accepted(
            "proxy with health check and interval",
            site(
                "app.test",
                "proxy = \"http://127.0.0.1:9\"\nhealth_check = \"/health\"\nhealth_interval = 5",
            ),
            loads,
        ),
    ]);
    rows
}

fn root_rows(roots: &SiteRoots) -> Vec<SiteRow> {
    let unreadable = roots.unreadable.display().to_string();
    vec![
        refused(
            "nonexistent root",
            site("static.test", &format!("root = \"{}\"", roots.missing)),
        ),
        refused(
            "unreadable root",
            site("static.test", &format!("root = \"{unreadable}\"")),
        ),
        refused(
            "root naming a file",
            site("static.test", &format!("root = \"{}\"", roots.file)),
        ),
        refused(
            "overlay with nonexistent root",
            site(
                "overlay.test",
                &format!(
                    "proxy = \"http://127.0.0.1:9\"\nroot = \"{}\"",
                    roots.missing
                ),
            ),
        ),
        accepted(
            "readable root",
            site("static.test", &format!("root = \"{}\"", roots.readable)),
            loads,
        ),
        accepted(
            "overlay with readable root",
            site(
                "overlay.test",
                &format!(
                    "proxy = \"http://127.0.0.1:9\"\nroot = \"{}\"\nhealth_check = \"/health\"",
                    roots.readable
                ),
            ),
            loads,
        ),
    ]
}

fn certificate_rows() -> Vec<SiteRow> {
    let tls_alpn = "[tls]\nauto = true\nemail = \"admin@example.com\"\n";
    vec![
        refused(
            "automatic TLS for an IPv4 site",
            format!("{tls_alpn}{}", proxy_site("127.0.0.1")),
        ),
        refused(
            "automatic TLS for an IPv6 site",
            format!("{tls_alpn}{}", proxy_site("[::1]:8443")),
        ),
        accepted(
            "automatic TLS names exclude the site port",
            format!("{tls_alpn}{}", proxy_site("app.example.com:8443")),
            |config| {
                let domains = config.auto_tls_domains();
                match &*domains {
                    ["app.example.com"] => Ok(()),
                    other => Err(format!(
                        "certificate names were {other:?}, expected [\"app.example.com\"]"
                    )),
                }
            },
        ),
    ]
}

fn run_site_row(row: &SiteRow) -> Result<(), String> {
    let file = write_config(&row.toml).map_err(|error| format!("write config: {error}"))?;
    let loaded = Config::load(file.path());
    file.close()
        .map_err(|error| format!("remove config: {error}"))?;
    match (loaded, &row.expectation) {
        (Err(_), SiteExpectation::Refused) => Ok(()),
        (Ok(_), SiteExpectation::Refused) => Err("Config::load accepted it".to_owned()),
        (Ok(config), SiteExpectation::Accepted(check)) => check(&config),
        (Err(error), SiteExpectation::Accepted(_)) => {
            Err(format!("Config::load refused a valid row: {error}"))
        }
    }
}

/// 8.T1: `Config::load` is the whole site admission. Each row loads on its
/// own; every failure is collected before the one declared assertion.
#[test]
fn site_configuration_rejects_every_invalid_authority_and_option() -> Result<(), FixtureError> {
    let roots = SiteRoots::create()?;
    let privileged = roots.unreadable_is_unreadable();
    let rows: Box<[SiteRow]> = [
        document_rows(),
        authority_rows(),
        proxy_rows(),
        health_rows(&roots),
        root_rows(&roots),
        certificate_rows(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let failures: Box<[String]> = rows
        .iter()
        .filter_map(|row| {
            run_site_row(row)
                .err()
                .map(|reason| format!("{}: {reason}", row.name))
        })
        .collect();
    roots.finish()?;
    privileged?;
    assert!(
        failures.is_empty(),
        "M9 invalid site configuration reached startup: {} of {} rows failed:\n{}",
        failures.len(),
        rows.len(),
        failures.join("\n")
    );
    Ok(())
}

/// Fields `camber serve --help` advertises: top level, `[tls]`, then site.
const ADVERTISED_FIELDS: [&str; 18] = [
    "listen",
    "connection_limit",
    "[tls]",
    "[[site]]",
    "cert",
    "key",
    "auto",
    "email",
    "staging",
    "cache_dir",
    "dns_provider",
    "dns_api_token_env",
    "dns_api_token_file",
    "host",
    "proxy",
    "root",
    "health_check",
    "health_interval",
];
/// The proxy constraints help states, each with a refused upstream that
/// breaks only that constraint and a secret the refusal must not repeat.
const PROXY_CONSTRAINTS: [(&str, &str, &str); 4] = [
    ("https", "ftp://127.0.0.1:9", "ftp://"),
    ("credentials", "http://user:hunter2@127.0.0.1:9", "hunter2"),
    ("query", "http://127.0.0.1:9/?token=hunter3", "hunter3"),
    ("fragment", "http://127.0.0.1:9/#hunter4", "hunter4"),
];
/// Providers help must never imply, spelled as an operator would try them.
const UNOFFERED_PROVIDERS: [&str; 4] = ["route53", "Cloudflare", "digitalocean", ""];
/// Claims the help must not make: no cloud deployment was certified.
const UNCERTIFIED_CLAIMS: [&str; 6] = [
    "certified",
    "certification",
    "production-ready",
    "route53",
    "route 53",
    "aws",
];
const HELP_DIAGNOSTIC: &str = "M9 CLI help disagrees with validated configuration";

/// `camber serve --help`, as an operator reads it.
fn serve_help() -> Result<String, FixtureError> {
    let mut command = Command::new(camber_bin());
    command.args(["serve", "--help"]);
    let output = run_command_with_timeout(command, CONFIG_REFUSAL_BOUND)?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        false => Err(FixtureError::new(format!(
            "camber serve --help exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))),
    }
}

/// Run `camber serve` on `toml`, which must be refused before any effect.
/// Returns the refusal's diagnostic, or why it was not a refusal.
fn serve_refusal(toml: &str) -> Result<String, String> {
    let root = tempfile::tempdir().map_err(|error| format!("temp root: {error}"))?;
    let config = root.path().join("camber.toml");
    std::fs::write(&config, toml).map_err(|error| format!("write config: {error}"))?;
    let mut command = serve_command(&config);
    command.env_remove("CAMBER_HELP_TOKEN");
    let output = run_command_within(command, CONFIG_REFUSAL_BOUND)
        .map_err(|error| format!("run the CLI: {error}"))?
        .ok_or_else(|| format!("the CLI did not exit within {CONFIG_REFUSAL_BOUND:?}"))?;
    root.close()
        .map_err(|error| format!("remove temp root: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    match (output.status.success(), output.announced_listener()) {
        (false, false) => Ok(stderr),
        (true, _) => Err(format!("the CLI accepted it: {stderr}")),
        (false, true) => Err(format!(
            "the CLI reported a listener before refusing: {stderr}"
        )),
    }
}

fn check_help_text(help: &str, failures: &mut Vec<String>) {
    for field in ADVERTISED_FIELDS
        .iter()
        .filter(|field| !help.contains(**field))
    {
        failures.push(format!(
            "help does not advertise the accepted field `{field}`"
        ));
    }
    if !help.contains("cloudflare") {
        failures.push("help does not name `cloudflare`, the only dns_provider".to_owned());
    }
    for (constraint, _, _) in PROXY_CONSTRAINTS {
        if !help.contains(constraint) {
            failures.push(format!(
                "help does not state the proxy constraint {constraint:?}"
            ));
        }
    }
    let words = spoken_words(help);
    for provider in UNOFFERED_PROVIDERS
        .iter()
        .filter(|provider| !provider.is_empty())
    {
        let lowered = provider.to_lowercase();
        if lowered != "cloudflare" && words.contains(&format!(" {lowered} ")) {
            failures.push(format!(
                "help implies the unsupported dns_provider {provider:?}"
            ));
        }
    }
    for claim in UNCERTIFIED_CLAIMS {
        if words.contains(&format!(" {claim} ")) {
            failures.push(format!("help claims {claim:?}, which no proof supports"));
        }
    }
}

/// `text` lowercased as space-separated words, padded so a phrase matches
/// only on word boundaries.
fn spoken_words(text: &str) -> String {
    let words: Box<[String]> = text
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    format!(" {} ", words.join(" "))
}

/// Every advertised site and DNS field loads together.
fn check_advertised_fields_load(failures: &mut Vec<String>) -> Result<(), FixtureError> {
    let root = tempfile::tempdir()?;
    let file = write_config(&format!(
        "listen = \"127.0.0.1:0\"\n\
         connection_limit = 16\n\
         [tls]\n\
         auto = true\n\
         email = \"admin@example.com\"\n\
         staging = true\n\
         cache_dir = \"{cache}\"\n\
         dns_provider = \"cloudflare\"\n\
         dns_api_token_env = \"CAMBER_HELP_TOKEN\"\n\
         [[site]]\n\
         host = \"app.example.com\"\n\
         proxy = \"https://backend.test/api\"\n\
         root = \"{root}\"\n\
         health_check = \"/health\"\n\
         health_interval = 5\n",
        cache = root.path().join("cache").display(),
        root = root.path().display(),
    ))?;
    if let Err(error) = Config::load(file.path()) {
        failures.push(format!(
            "a config of advertised fields was refused: {error}"
        ));
    }
    file.close()?;
    root.close()?;
    Ok(())
}

/// Each provider help does not offer is refused before effects, and the
/// refusal names the field and the one advertised choice.
fn check_provider_refusals(failures: &mut Vec<String>) {
    for provider in UNOFFERED_PROVIDERS {
        let toml = format!(
            "listen = \"127.0.0.1:0\"\n\
             [tls]\n\
             auto = true\n\
             email = \"admin@example.com\"\n\
             dns_provider = \"{provider}\"\n\
             dns_api_token_env = \"CAMBER_HELP_TOKEN\"\n\
             [[site]]\n\
             host = \"example.com\"\n\
             proxy = \"http://127.0.0.1:9\"\n"
        );
        match serve_refusal(&toml) {
            Ok(stderr) if stderr.contains("dns_provider") && stderr.contains("cloudflare") => {}
            Ok(stderr) => failures.push(format!(
                "dns_provider {provider:?}: the refusal does not name dns_provider and \
                 its only choice cloudflare: {stderr}"
            )),
            Err(reason) => failures.push(format!("dns_provider {provider:?}: {reason}")),
        }
    }
}

/// Each refused proxy names the constraint help states, never its secret.
fn check_proxy_refusals(failures: &mut Vec<String>) {
    for (constraint, proxy, secret) in PROXY_CONSTRAINTS {
        let toml = format!(
            "listen = \"127.0.0.1:0\"\n[[site]]\nhost = \"app.test\"\nproxy = \"{proxy}\"\n"
        );
        match serve_refusal(&toml) {
            Ok(stderr)
                if stderr.contains("proxy")
                    && stderr.contains(constraint)
                    && !stderr.contains(secret) => {}
            Ok(stderr) => failures.push(format!(
                "proxy {constraint:?}: the refusal must name proxy and {constraint:?} \
                 without repeating {secret:?}: {stderr}"
            )),
            Err(reason) => failures.push(format!("proxy {constraint:?}: {reason}")),
        }
    }
}

/// An unadvertised field is refused by name.
fn check_unadvertised_field(failures: &mut Vec<String>) {
    let toml = "listen = \"127.0.0.1:0\"\n[[site]]\nhost = \"app.test\"\n\
                proxy = \"http://127.0.0.1:9\"\nupstream_pool = 4\n";
    match serve_refusal(toml) {
        Ok(stderr) if stderr.contains("upstream_pool") => {}
        Ok(stderr) => failures.push(format!(
            "the unadvertised field refusal does not name upstream_pool: {stderr}"
        )),
        Err(reason) => failures.push(format!("unadvertised field: {reason}")),
    }
}

/// 21.T2: what `camber serve --help` advertises is what validation accepts,
/// and what it refuses is refused by name before any effect.
#[test]
fn integration_help_matches_validated_configuration() -> Result<(), FixtureError> {
    let help = serve_help()?;
    let mut failures = Vec::new();
    check_help_text(&help, &mut failures);
    check_advertised_fields_load(&mut failures)?;
    check_provider_refusals(&mut failures);
    check_proxy_refusals(&mut failures);
    check_unadvertised_field(&mut failures);
    assert!(
        failures.is_empty(),
        "{HELP_DIAGNOSTIC}: {} disagreements:\n{}\n--- camber serve --help ---\n{help}",
        failures.len(),
        failures.join("\n")
    );
    Ok(())
}
