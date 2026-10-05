//! 7.T1 rows: every ACME name set and TLS mode is validated before any
//! effect. The test in `acme_configuration` runs each family.

use crate::integration_rows::{
    Row, all, bounded, expect, expect_eq, expect_ok, is_configuration_refusal, tempdir,
};
use camber::AcmeConfig;
use camber::config::{TlsConfig, load_config};
use camber::dns01::{AcmeDns01, DnsProvider, RecordId};
use camber::{RuntimeError, runtime};
use std::fmt::Display;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Hang guard for one refused or credential-failed provisioning call.
const BOUND: Duration = Duration::from_secs(10);
/// The DNS-01 order cap on configured domains.
const DNS01_DOMAIN_CAP: usize = 100;
/// Malformed account credentials: reading them fails without the network.
const MALFORMED_CREDENTIALS: &[u8] = b"{ not account credentials";

/// One named domain set.
struct NameSet {
    label: Box<str>,
    names: Box<[Box<str>]>,
}

fn set(label: &str, names: &[&str]) -> NameSet {
    NameSet {
        label: label.into(),
        names: names.iter().map(|name| Box::from(*name)).collect(),
    }
}

fn owned_set(label: &str, names: impl IntoIterator<Item = String>) -> NameSet {
    NameSet {
        label: label.into(),
        names: names.into_iter().map(String::into_boxed_str).collect(),
    }
}

/// A name of exactly `octets` octets, in labels of at most 63.
fn name_of_length(octets: usize) -> String {
    let mut labels = Vec::new();
    let mut remaining = octets;
    while remaining > 64 {
        labels.push("a".repeat(63));
        remaining -= 64;
    }
    labels.push("b".repeat(remaining));
    labels.join(".")
}

fn distinct_domains(count: usize) -> Vec<String> {
    (0..count)
        .map(|index| format!("d{index}.example.com"))
        .collect()
}

/// Sets both challenge types refuse.
fn invalid_everywhere() -> Vec<NameSet> {
    vec![
        set("empty domain set", &[]),
        set("empty name", &[""]),
        set("root only", &["."]),
        set("empty inner label", &["example..com"]),
        set("leading dot", &[".example.com"]),
        set("two trailing dots", &["example.com.."]),
        set("leading hyphen", &["-example.com"]),
        set("trailing hyphen", &["example-.com"]),
        set("underscore", &["exa_mple.com"]),
        set("inner whitespace", &["exa mple.com"]),
        set("leading whitespace", &[" example.com"]),
        set("trailing newline", &["example.com\n"]),
        set("port", &["example.com:443"]),
        set("URL", &["https://example.com"]),
        set("IDNA U-label", &["bücher.example"]),
        owned_set("64-octet label", [format!("{}.example", "a".repeat(64))]),
        owned_set("254-octet name", [name_of_length(254)]),
        set("IPv4 literal", &["192.0.2.1"]),
        set("IPv4 literal with trailing dot", &["192.0.2.1."]),
        set("IPv6 literal", &["2001:db8::1"]),
        set("bracketed IPv6 literal", &["[2001:db8::1]"]),
        set("bare wildcard", &["*"]),
        set("wildcard of the root", &["*."]),
        set("embedded wildcard label", &["foo.*.example.com"]),
        set("partial wildcard label", &["*example.com"]),
        set("suffixed wildcard label", &["w*.example.com"]),
        set("double wildcard", &["*.*.example.com"]),
        set("doubled star", &["**.example.com"]),
        set("exact duplicate", &["example.com", "example.com"]),
        set("case duplicate", &["example.com", "EXAMPLE.COM"]),
        set("trailing-dot duplicate", &["example.com", "example.com."]),
        set("case and dot duplicate", &["Example.Com.", "example.COM"]),
    ]
}

/// Sets both challenge types accept.
fn valid_everywhere() -> Vec<NameSet> {
    vec![
        set("one name", &["example.com"]),
        set("uppercase normalizes", &["Example.COM"]),
        set("one trailing dot", &["example.com."]),
        set("IDNA A-label", &["xn--bcher-kva.example"]),
        set("inner hyphen and digit", &["a-b.c1.example"]),
        set("numeric label", &["123.example.com"]),
        owned_set("63-octet label", [format!("{}.example", "a".repeat(63))]),
        owned_set("253-octet name", [name_of_length(253)]),
        set("distinct names", &["example.com", "www.example.com"]),
    ]
}

fn tls_alpn01_invalid() -> Vec<NameSet> {
    let mut sets = invalid_everywhere();
    sets.extend([
        set("TLS-ALPN-01 wildcard", &["*.example.com"]),
        set(
            "TLS-ALPN-01 wildcard beside apex",
            &["example.com", "*.example.com"],
        ),
        set(
            "TLS-ALPN-01 wildcard with trailing dot",
            &["*.example.com."],
        ),
    ]);
    sets
}

fn dns01_invalid() -> Vec<NameSet> {
    let mut sets = invalid_everywhere();
    sets.extend([
        set(
            "normalized wildcard duplicate",
            &["*.example.com", "*.EXAMPLE.com."],
        ),
        owned_set(
            "one domain over the DNS-01 cap",
            distinct_domains(DNS01_DOMAIN_CAP + 1),
        ),
    ]);
    sets
}

fn dns01_valid() -> Vec<NameSet> {
    let mut sets = valid_everywhere();
    sets.extend([
        set("DNS-01 wildcard", &["*.example.com"]),
        set(
            "DNS-01 wildcard beside apex",
            &["example.com", "*.example.com"],
        ),
        set("DNS-01 wildcard with trailing dot", &["*.example.com."]),
        owned_set("exactly the DNS-01 cap", distinct_domains(DNS01_DOMAIN_CAP)),
    ]);
    sets
}

/// A configuration refusal, in either the untyped or the integration form.
fn expect_refused<T>(what: &str, outcome: &Result<T, RuntimeError>) -> Row {
    match outcome {
        Ok(_) => Err(format!("{what}: accepted")),
        Err(error) => expect(
            &format!("{what}: refused as {error:?}, not as configuration"),
            is_configuration_refusal(error),
        ),
    }
}

fn expect_not_refused<T>(what: &str, outcome: &Result<T, RuntimeError>) -> Row {
    match outcome {
        Ok(_) => Ok(()),
        Err(error) => expect(
            &format!("{what}: a valid set was refused as configuration: {error:?}"),
            !is_configuration_refusal(error),
        ),
    }
}

/// `outcome` judged as configuration that is `valid`: a valid one must be
/// accepted, an invalid one refused as configuration.
fn verdict<T>(what: &str, outcome: Result<T, RuntimeError>, valid: bool) -> Row {
    match valid {
        true => expect_ok(what, outcome).map(drop),
        false => expect_refused(what, &outcome),
    }
}

/// Judge every labelled invalid and valid item with `row`, and name each
/// failure.
fn verdicts<L: Display, T>(
    invalid: impl IntoIterator<Item = (L, T)>,
    valid: impl IntoIterator<Item = (L, T)>,
    row: impl Fn(T, bool) -> Row,
) -> Row {
    let invalid = invalid.into_iter().map(|(label, item)| {
        row(item, false).map_err(|reason| format!("invalid {label}: {reason}"))
    });
    let valid = valid
        .into_iter()
        .map(|(label, item)| row(item, true).map_err(|reason| format!("valid {label}: {reason}")));
    all(invalid.chain(valid))
}

/// Name sets as labelled items.
fn labelled(sets: Vec<NameSet>) -> impl Iterator<Item = (Box<str>, Box<[Box<str>]>)> {
    sets.into_iter().map(|set| (set.label, set.names))
}

type CacheSnapshot = Box<[(Box<str>, Box<[u8]>)]>;

/// Every entry in `dir` with its bytes, or `None` when it is absent.
fn cache_snapshot(dir: &Path) -> Result<Option<CacheSnapshot>, String> {
    match dir.try_exists() {
        Ok(false) => return Ok(None),
        Ok(true) => {}
        Err(error) => return Err(format!("probe {}: {error}", dir.display())),
    }
    let entries = std::fs::read_dir(dir).map_err(|error| format!("list cache: {error}"))?;
    let mut snapshot = entries
        .map(|entry| {
            let entry = entry.map_err(|error| format!("read cache entry: {error}"))?;
            let name = entry
                .file_name()
                .to_string_lossy()
                .into_owned()
                .into_boxed_str();
            match std::fs::read(entry.path()) {
                Ok(bytes) => Ok((name, bytes.into_boxed_slice())),
                Err(error) => Err(format!("read cache entry {name}: {error}")),
            }
        })
        .collect::<Result<Vec<_>, String>>()?;
    snapshot.sort();
    Ok(Some(snapshot.into_boxed_slice()))
}

/// TLS-ALPN-01 name sets, judged at `AcmeConfig::build`.
pub fn tls_alpn01_rows() -> Row {
    verdicts(
        labelled(tls_alpn01_invalid()),
        labelled(valid_everywhere()),
        tls_alpn01_row,
    )
}

fn tls_alpn01_row(names: Box<[Box<str>]>, valid: bool) -> Row {
    let root = tempdir()?;
    let cache = root.path().join("cache");
    let outcome = AcmeConfig::new("camber", names)
        .email("admin@example.com")
        .staging(true)
        .cache_dir(&cache)
        .build();
    let after = cache_snapshot(&cache)?;
    match valid {
        true => verdict("build", outcome, true),
        false => all([
            expect_refused("build", &outcome),
            expect_eq("the cache after refusal", after, None),
        ]),
    }
}

/// Counts every provider call: a refused set must show zero. Preparation
/// is counted apart, because a valid set is prepared before credentials
/// load while no record may be written before them.
#[derive(Clone, Default)]
struct RecordingProvider {
    calls: Arc<AtomicUsize>,
    prepares: Arc<AtomicUsize>,
}

impl RecordingProvider {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn prepares(&self) -> usize {
        self.prepares.load(Ordering::SeqCst)
    }
}

impl DnsProvider for RecordingProvider {
    fn prepare(
        &mut self,
        _domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(()))
    }

    fn create_txt_record(
        &self,
        _fqdn: &str,
        _value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(RecordId::from("recorded")))
    }

    fn delete_txt_record(
        &self,
        _record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(()))
    }
}

/// DNS-01 name sets, judged at `provision_cert`.
pub fn dns01_rows() -> Row {
    let outcome = runtime::builder().run(|| {
        verdicts(
            labelled(dns01_invalid()),
            labelled(dns01_valid()),
            dns01_row,
        )
    });
    match outcome {
        Ok(verdict) => verdict,
        Err(error) => Err(format!("the runtime tore down with {error:?}")),
    }
}

fn dns01_row(names: Box<[Box<str>]>, valid: bool) -> Row {
    let root = tempdir()?;
    let cache = root.path().join("cache");
    std::fs::create_dir(&cache).map_err(|error| format!("create cache: {error}"))?;
    std::fs::write(cache.join("account.json"), MALFORMED_CREDENTIALS)
        .map_err(|error| format!("seed credentials: {error}"))?;
    let before = cache_snapshot(&cache)?;
    let provider = RecordingProvider::default();
    let acme = AcmeDns01::new("camber", names)
        .email("admin@example.com")
        .staging(true)
        .cache_dir(&cache);
    let outcome = bounded(
        "provision_cert",
        BOUND,
        acme.provision_cert(provider.clone()),
    )?;
    let after = cache_snapshot(&cache)?;
    match valid {
        true => all([
            expect_not_refused("provision_cert", &outcome),
            expect_eq("provider calls before credentials", provider.calls(), 0),
        ]),
        false => all([
            expect_refused("provision_cert", &outcome),
            expect_eq("provider calls after refusal", provider.calls(), 0),
            expect_eq("preparations after refusal", provider.prepares(), 0),
            expect_eq("the cache after refusal", after, before),
        ]),
    }
}

fn manual() -> TlsConfig {
    TlsConfig {
        cert: Some("/etc/camber/cert.pem".into()),
        key: Some("/etc/camber/key.pem".into()),
        auto: None,
        email: None,
        staging: None,
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    }
}

fn automatic() -> TlsConfig {
    TlsConfig {
        cert: None,
        key: None,
        auto: Some(true),
        email: Some("admin@example.com".into()),
        staging: None,
        cache_dir: None,
        dns_provider: None,
        dns_api_token_env: None,
        dns_api_token_file: None,
    }
}

fn dns(provider: &str) -> TlsConfig {
    TlsConfig {
        dns_provider: Some(provider.into()),
        dns_api_token_env: Some("CF_TOKEN".into()),
        ..automatic()
    }
}

/// `TlsConfig` modes and provider names, judged at `validate`.
pub fn tls_mode_rows() -> Row {
    let invalid = [
        ("unknown provider", dns("route53")),
        ("empty provider", dns("")),
        ("uppercase provider", dns("Cloudflare")),
        ("padded provider", dns(" cloudflare")),
        (
            "manual mode with email",
            TlsConfig {
                email: Some("admin@example.com".into()),
                ..manual()
            },
        ),
        (
            "manual mode with staging",
            TlsConfig {
                staging: Some(true),
                ..manual()
            },
        ),
        (
            "manual mode with cache_dir",
            TlsConfig {
                cache_dir: Some("/var/cache/camber".into()),
                ..manual()
            },
        ),
    ];
    let valid = [
        ("manual mode", manual()),
        (
            "manual mode with auto off",
            TlsConfig {
                auto: Some(false),
                ..manual()
            },
        ),
        ("automatic mode", automatic()),
        (
            "automatic mode with staging and cache_dir",
            TlsConfig {
                staging: Some(true),
                cache_dir: Some("/var/cache/camber".into()),
                ..automatic()
            },
        ),
        ("cloudflare with an env token", dns("cloudflare")),
        (
            "cloudflare with a file token",
            TlsConfig {
                dns_api_token_env: None,
                dns_api_token_file: Some("/run/secrets/cf-token".into()),
                ..dns("cloudflare")
            },
        ),
    ];
    verdicts(invalid, valid, |tls: TlsConfig, valid| {
        verdict("validate", tls.validate(), valid)
    })
}

fn load_and_validate(toml: &str) -> Result<Result<(), RuntimeError>, String> {
    let root = tempdir()?;
    let path = root.path().join("tls.toml");
    std::fs::write(&path, toml).map_err(|error| format!("write config: {error}"))?;
    Ok(load_config::<TlsConfig>(&path).and_then(|tls| tls.validate()))
}

/// Unknown `TlsConfig` input fields, judged at load.
pub fn tls_field_rows() -> Row {
    let invalid = [
        (
            "misspelled provider field",
            "auto = true\nemail = \"admin@example.com\"\ndns_providr = \"cloudflare\"\n",
        ),
        (
            "unknown manual field",
            "cert = \"/etc/camber/cert.pem\"\nkey = \"/etc/camber/key.pem\"\nchain = \"/etc/camber/chain.pem\"\n",
        ),
        (
            "unknown domains field",
            "auto = true\nemail = \"admin@example.com\"\ndomains = [\"example.com\"]\n",
        ),
    ];
    let valid = [
        (
            "automatic fields",
            "auto = true\nemail = \"admin@example.com\"\nstaging = true\n",
        ),
        (
            "manual fields",
            "cert = \"/etc/camber/cert.pem\"\nkey = \"/etc/camber/key.pem\"\n",
        ),
        (
            "cloudflare fields",
            "auto = true\nemail = \"admin@example.com\"\ndns_provider = \"cloudflare\"\ndns_api_token_env = \"CF_TOKEN\"\n",
        ),
    ];
    verdicts(invalid, valid, |toml, valid| {
        load_and_validate(toml).and_then(|outcome| verdict("load", outcome, valid))
    })
}
