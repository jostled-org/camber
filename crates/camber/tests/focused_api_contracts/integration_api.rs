//! The published integration surface, compiled under each optional feature.
//!
//! Each feature's probe is gated on that feature alone, so this root compiles
//! the documented shapes under the default set, under `nats`, `sqs`, `dns01`,
//! or `grpc` in isolation, and under the combined set. A probe builds its
//! futures and drops them unpolled: it proves the shape, never a wire effect.
//!
//! The reference and the probes must agree. Every example in
//! `docs/reference/integrations.md` may name only `camber` paths a probe here
//! uses, and only integration functions a probe here calls. The support
//! matrix names exactly the features probed here. Removed spellings are
//! absence checks over the public surfaces, not compatibility shims. The
//! tracked workflow compiles this root under each isolated feature.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(feature = "dns01")]
use std::future::Future;
#[cfg(feature = "dns01")]
use std::sync::Arc;
#[cfg(any(feature = "nats", feature = "sqs"))]
use std::time::Duration;

#[cfg(feature = "dns01")]
use camber::dns01::{AcmeDns01, CloudflareProvider};
#[cfg(feature = "dns01")]
use camber::dns01::{DnsProvider, RecordId};
#[cfg(feature = "grpc")]
use camber::http::{GrpcRouter, Router};
#[cfg(feature = "nats")]
use camber::mq::nats;
#[cfg(feature = "sqs")]
use camber::mq::sqs;
#[cfg(feature = "dns01")]
use camber::{CertStore, RuntimeError};

use crate::delivery_fixture::{repository_root, repository_text};
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
use crate::probes::require_send;

const REFERENCE: &str = "docs/reference/integrations.md";
const DIAGNOSTIC: &str = "M9 published integration contract disagrees with its compiled shapes";
/// This file: the probes the reference must agree with.
const PROBES: &str = include_str!("integration_api.rs");
/// The integration owners whose public functions a reference example may
/// call only when a probe here calls them too.
const INTEGRATION_SOURCES: [&str; 3] = [
    "crates/camber/src/mq",
    "crates/camber/src/dns01",
    "crates/camber/src/http/grpc_support.rs",
];
/// Each optional integration feature, and what its matrix row must state.
const MATRIX: [(&str, &[&str]); 4] = [
    ("nats", &["Core NATS", "JetStream"]),
    ("sqs", &["SQS", "Standard", "FIFO"]),
    ("dns01", &["Cloudflare", "DnsProvider"]),
    (
        "grpc",
        &[
            "unary",
            "client-streaming",
            "server-streaming",
            "bidirectional",
        ],
    ),
];
/// Public surfaces no removed spelling may reach.
const PUBLIC_SURFACES: [&str; 5] = [
    "crates/camber/src",
    "crates/camber-cli/src",
    "README.md",
    "docs/reference",
    "docs/guides",
];
/// The tracked entry points that must run the isolated feature builds.
const WORKFLOW_ENTRIES: [&str; 2] = [
    ".github/workflows/ci.yml",
    ".github/scripts/reproduce-ci.sh",
];

/// Spellings the breaking release removed. Assembled so that no scan reads
/// them back out of this file.
fn removed_spellings() -> [&'static str; 9] {
    [
        concat!("connect", "_async"),
        concat!("publish", "_async"),
        concat!("subscribe", "_async"),
        concat!("queue_subscribe", "_async"),
        concat!("send_message", "_async"),
        concat!("receive_messages", "_async"),
        concat!("delete_message", "_async"),
        concat!("mq::", "blocking"),
        concat!("mod ", "blocking"),
    ]
}

// --- Probes --------------------------------------------------------------

/// The Core NATS example the reference prints.
#[cfg(feature = "nats")]
async fn nats_notify() -> Result<(), camber::RuntimeError> {
    let connection = nats::builder("nats://127.0.0.1:4222")
        .operation_timeout(Duration::from_secs(5))
        .max_subscriptions(8)
        .connect()
        .await?;
    let mut orders = connection.subscribe("orders.created").await?;
    connection.publish("orders.created", b"order 7").await?;
    if let Some(message) = orders.next().await? {
        println!("{}", String::from_utf8_lossy(message.payload()));
    }
    connection.close().await
}

/// The acknowledged NATS example the reference prints.
#[cfg(feature = "nats")]
async fn nats_record(payload: &[u8]) -> Result<(), camber::RuntimeError> {
    let connection = nats::builder("nats://127.0.0.1:4222")
        .acknowledged_publishing("EVENTS")
        .connect()
        .await?;
    connection.publish("events.created", payload).await?;
    connection.close().await
}

/// Every other public Core NATS spelling, once each.
#[cfg(feature = "nats")]
async fn nats_every_operation(url: &str) -> Result<(), camber::RuntimeError> {
    let connection = nats::builder(url)
        .connect_timeout(Duration::from_secs(10))
        .shutdown_timeout(Duration::from_secs(5))
        .max_in_flight(64)
        .max_message_bytes(1 << 20)
        .subscription_capacity(64)
        .client_capacity(64)
        .connect()
        .await?;
    connection.ready()?;
    let defaults: nats::Connection = nats::connect(url).await?;
    let mut workers: nats::Subscription = connection.queue_subscribe("jobs", "workers").await?;
    let immediate: Option<nats::Message> = workers.try_next()?;
    let waited = workers.next_timeout(Duration::from_secs(1)).await?;
    for message in immediate.iter().chain(waited.iter()) {
        let _: (&str, &[u8]) = (message.subject(), message.payload());
    }
    workers.close().await?;
    defaults.close().await?;
    connection.close().await
}

/// The Amazon SQS example the reference prints.
#[cfg(feature = "sqs")]
async fn sqs_drain_one(queue: &str) -> Result<(), camber::RuntimeError> {
    let client = sqs::builder()
        .region("us-east-1")
        .operation_timeout(Duration::from_secs(25))
        .connect()
        .await?;
    client.ready(queue).await?;
    client.send_message(queue, "order 7").await?;
    for message in client
        .receive_messages(queue, 10, Duration::from_secs(20))
        .await?
    {
        if let Some(receipt) = message.receipt_handle() {
            client.delete_message(queue, receipt).await?;
        }
    }
    client.close().await
}

/// Every other public SQS spelling, once each.
#[cfg(feature = "sqs")]
async fn sqs_every_operation(queue: &str) -> Result<(), camber::RuntimeError> {
    let client: sqs::Client = sqs::builder()
        .endpoint("http://127.0.0.1:9324")
        .credentials("camber-local", "camber-local-secret", None)
        .connect_timeout(Duration::from_secs(10))
        .shutdown_timeout(Duration::from_secs(5))
        .max_in_flight(64)
        .max_message_bytes(1 << 20)
        .connect()
        .await?;
    let defaults: sqs::Client = sqs::connect().await?;
    let _: Box<str> = client.send_message(queue, "order 8").await?;
    let batch: Box<[sqs::Message]> = client
        .receive_messages(queue, 1, Duration::from_secs(1))
        .await?;
    for message in batch.iter() {
        let _: [Option<&str>; 3] = [
            message.body(),
            message.message_id(),
            message.receipt_handle(),
        ];
    }
    defaults.close().await?;
    client.close().await
}

/// A custom provider: the explicit Rust interface, owned by the order.
#[cfg(feature = "dns01")]
struct ZoneFile {
    prepared: usize,
}

#[cfg(feature = "dns01")]
impl DnsProvider for ZoneFile {
    fn prepare(
        &mut self,
        domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.prepared = domains.len();
        async { Ok(()) }
    }

    fn create_txt_record(
        &self,
        fqdn: &str,
        value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        let id: RecordId = format!("{fqdn}={value}").into();
        let prepared = self.prepared;
        async move {
            match prepared {
                0 => Err(RuntimeError::Config("no prepared authority".into())),
                _ => Ok(id),
            }
        }
    }

    fn delete_txt_record(
        &self,
        record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        let known = !record_id.is_empty();
        async move {
            match known {
                true => Ok(()),
                false => Err(RuntimeError::Config("unknown record".into())),
            }
        }
    }
}

/// Pure descriptors: synchronous, domain-free, and fallible.
#[cfg(feature = "dns01")]
fn dns_descriptors() -> [Result<CloudflareProvider, RuntimeError>; 2] {
    [
        CloudflareProvider::new("dummy-token".into()),
        CloudflareProvider::with_base_url("dummy-token".into(), "http://127.0.0.1:9".into()),
    ]
}

/// The validated issuer, with local trust for a loopback directory.
#[cfg(feature = "dns01")]
fn dns_issuer(root_pem: &[u8]) -> Result<AcmeDns01, RuntimeError> {
    let issuer = AcmeDns01::new("camber", ["example.com", "*.example.com"])
        .email("admin@example.com")
        .cache_dir("/tmp/camber-dns01")
        .staging(true)
        .directory_url("https://127.0.0.1:14000/dir")?
        .add_root_certificate(root_pem)?;
    issuer.validate()?;
    Ok(issuer)
}

/// Owned provisioning and renewal, with Cloudflare and a custom provider.
#[cfg(feature = "dns01")]
async fn dns_every_operation(issuer: AcmeDns01, token: Box<str>) -> Result<(), RuntimeError> {
    let _: &Path = issuer.cache_path();
    let cached = issuer.load_cached_cert()?;
    let renew = issuer.needs_renewal();
    let custom = ZoneFile { prepared: 0 };
    let issued = issuer.provision_cert(custom).await?;
    let provider = CloudflareProvider::new(token)?;
    let store = CertStore::new(issued);
    let renewal = issuer.spawn_renewal(provider, store);
    let _: (Option<_>, bool) = (cached, renew);
    renewal.await?
}

/// A native tonic service mounted beside ordinary HTTP routes.
#[cfg(feature = "grpc")]
fn grpc_mount() -> Router {
    let (_reporter, health) = tonic_health::server::health_reporter();
    let mut router = Router::new();
    router.grpc(GrpcRouter::new().add_service(health));
    router
}

/// Builds every probe that the compiled feature set includes.
fn compile_feature_probes() {
    #[cfg(feature = "nats")]
    {
        drop(require_send(nats_notify()));
        drop(require_send(nats_record(b"order 7")));
        drop(require_send(nats_every_operation("nats://127.0.0.1:4222")));
    }
    #[cfg(feature = "sqs")]
    {
        drop(require_send(sqs_drain_one("http://127.0.0.1:9324/q")));
        drop(require_send(sqs_every_operation("http://127.0.0.1:9324/q")));
    }
    #[cfg(feature = "dns01")]
    {
        let _: fn() -> [Result<CloudflareProvider, RuntimeError>; 2] = dns_descriptors;
        let _: fn(&[u8]) -> Result<AcmeDns01, RuntimeError> = dns_issuer;
        let issuer = AcmeDns01::new("camber", ["example.com"]);
        drop(require_send(dns_every_operation(
            issuer,
            "dummy-token".into(),
        )));
    }
    #[cfg(feature = "grpc")]
    {
        let _: fn() -> Router = grpc_mount;
    }
}

// --- Reference agreement -------------------------------------------------

/// The text of one scanned file. An unreadable file fails the scan rather
/// than leaving it unread.
fn scanned_text(path: &Path) -> Box<str> {
    fs::read_to_string(path)
        .map(String::into_boxed_str)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// Every fenced Rust example in `text`.
fn rust_fences(text: &str) -> Box<[Box<str>]> {
    let mut fences = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        match (current.take(), line.trim_start().strip_prefix("```")) {
            (None, Some(info)) => current = info.starts_with("rust").then(String::new),
            (Some(body), Some(_)) => fences.push(body.into_boxed_str()),
            (Some(mut body), None) => {
                body.push_str(line);
                body.push('\n');
                current = Some(body);
            }
            (None, None) => {}
        }
    }
    fences.into_boxed_slice()
}

/// Every source file below `relative`, or `relative` itself when a file.
fn source_files(relative: &str) -> Box<[PathBuf]> {
    let root = repository_root().join(relative);
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    files.into_boxed_slice()
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    match path.is_dir() {
        true => {
            let entries = fs::read_dir(path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            for entry in entries {
                let entry = entry.expect("source directory entry was not readable");
                collect_files(&entry.path(), files);
            }
        }
        false if path.is_file() => files.push(path.to_path_buf()),
        false => panic!("public surface {} is absent", path.display()),
    }
}

fn identifier_after<'a>(text: &'a str, marker: &str) -> impl Iterator<Item = &'a str> {
    text.match_indices(marker).filter_map(move |(at, _)| {
        let rest = &text[at + marker.len()..];
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        (end > 0).then(|| &rest[..end])
    })
}

/// Every public function name the integration owners declare.
fn integration_functions() -> BTreeSet<Box<str>> {
    INTEGRATION_SOURCES
        .iter()
        .flat_map(|relative| source_files(relative))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .flat_map(|path| {
            let text = scanned_text(&path);
            ["pub fn ", "pub async fn "]
                .into_iter()
                .flat_map(|marker| identifier_after(&text, marker).map(Box::from))
                .collect::<Box<[_]>>()
        })
        .collect()
}

/// Each `camber::` path a fence names must be one a probe names.
fn fence_paths_are_probed(index: usize, fence: &str, failures: &mut Vec<String>) {
    for line in fence.lines().map(str::trim) {
        let Some(path) = line.strip_prefix("use ") else {
            continue;
        };
        let path = path.trim_end_matches(';');
        if path.starts_with("camber") && !PROBES.contains(path) {
            failures.push(format!(
                "{REFERENCE} example {index} uses `{path}`, which no probe here uses"
            ));
        }
    }
}

/// Each integration function a fence calls must be one a probe calls.
fn fence_calls_are_probed(
    index: usize,
    fence: &str,
    functions: &BTreeSet<Box<str>>,
    failures: &mut Vec<String>,
) {
    for marker in [".", "::"] {
        let unprobed = identifier_after(fence, marker)
            .filter(|name| functions.contains(*name))
            .map(|name| format!("{marker}{name}("))
            .filter(|call| fence.contains(call.as_str()) && !PROBES.contains(call.as_str()));
        for call in unprobed {
            failures.push(format!(
                "{REFERENCE} example {index} calls `{call}`, which no probe here compiles"
            ));
        }
    }
}

fn check_reference_examples(reference: &str, failures: &mut Vec<String>) {
    let fences = rust_fences(reference);
    if fences.is_empty() {
        failures.push(format!("{REFERENCE} prints no Rust example"));
    }
    let functions = integration_functions();
    for (index, fence) in fences.iter().enumerate() {
        fence_paths_are_probed(index, fence, failures);
        fence_calls_are_probed(index, fence, &functions, failures);
    }
}

/// The support matrix rows, keyed by their backticked feature column.
fn matrix_rows(reference: &str) -> Box<[(&str, &str)]> {
    let section = reference
        .split_once("## Support Matrix")
        .map(|(_, rest)| rest.split("\n## ").next().unwrap_or_default())
        .unwrap_or_default();
    section
        .lines()
        .filter(|line| line.starts_with('|'))
        .filter_map(|line| {
            let feature = line.split('|').nth(2)?.trim();
            let feature = feature.strip_prefix('`')?.strip_suffix('`')?;
            Some((feature, line))
        })
        .collect()
}

fn check_support_matrix(reference: &str, failures: &mut Vec<String>) {
    let rows = matrix_rows(reference);
    let listed: BTreeSet<&str> = rows.iter().map(|(feature, _)| *feature).collect();
    let probed: BTreeSet<&str> = MATRIX.iter().map(|(feature, _)| *feature).collect();
    if listed != probed {
        failures.push(format!(
            "{REFERENCE} support matrix lists features {listed:?}; the probes cover {probed:?}"
        ));
    }
    for (feature, labels) in MATRIX {
        let row = rows
            .iter()
            .find(|(listed, _)| *listed == feature)
            .map(|(_, row)| *row)
            .unwrap_or_default();
        for label in labels.iter().filter(|label| !row.contains(**label)) {
            failures.push(format!(
                "{REFERENCE} support matrix row for `{feature}` does not state {label:?}: {row:?}"
            ));
        }
    }
}

// --- Removed spellings and feature builds --------------------------------

fn check_removed_spellings(failures: &mut Vec<String>) {
    let blocking = repository_root().join("crates/camber/src/mq/blocking.rs");
    if blocking.exists() {
        failures.push(format!(
            "the implicit blocking MQ bridge {} remains",
            blocking.display()
        ));
    }
    for path in PUBLIC_SURFACES
        .iter()
        .flat_map(|relative| source_files(relative))
    {
        let text = scanned_text(&path);
        for spelling in removed_spellings()
            .into_iter()
            .filter(|spelling| text.contains(*spelling))
        {
            failures.push(format!(
                "{} still names the removed spelling `{spelling}`",
                path.display()
            ));
        }
        for line in text
            .lines()
            .filter(|line| awaits_cloudflare_constructor(line))
        {
            failures.push(format!(
                "{} still awaits a Cloudflare constructor: {}",
                path.display(),
                line.trim()
            ));
        }
    }
}

/// Whether `line` awaits a Cloudflare constructor call directly: the removed
/// async, lookup-performing construction.
fn awaits_cloudflare_constructor(line: &str) -> bool {
    line.match_indices("CloudflareProvider::").any(|(at, _)| {
        let rest = &line[at..];
        let Some(open) = rest.find('(') else {
            return false;
        };
        let mut depth = 0_usize;
        for (offset, c) in rest[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' if depth == 1 => {
                    return rest[open + offset + 1..].starts_with(".await");
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        false
    })
}

/// Lowercase identifier tokens in `text`.
fn tokens(text: &str) -> BTreeSet<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
        .collect()
}

/// The tracked file that runs an isolated feature check of the library and
/// this probe root for every optional integration, if any does. The probes
/// compile here only under the combined set; that file proves each alone.
fn feature_build_owner() -> Option<Box<str>> {
    source_files(".github")
        .into_iter()
        .filter_map(|path| {
            let text = scanned_text(&path);
            let checks = text.lines().any(|line| {
                [
                    "cargo",
                    "check",
                    "--lib",
                    "--test focused_api_contracts",
                    "--no-default-features",
                ]
                .iter()
                .all(|part| line.contains(part))
            });
            let owner: Box<str> = path.file_name()?.to_string_lossy().into();
            let named = tokens(&text);
            let isolates_each = MATRIX.iter().all(|(feature, _)| named.contains(feature));
            (checks && isolates_each).then_some(owner)
        })
        .next()
}

fn check_feature_builds(failures: &mut Vec<String>) {
    let Some(owner) = feature_build_owner() else {
        failures.push(
            "no tracked workflow file runs `cargo check -p camber --lib --test \
             focused_api_contracts --no-default-features` for each of nats, sqs, \
             dns01, and grpc"
                .to_owned(),
        );
        return;
    };
    for entry in WORKFLOW_ENTRIES {
        let text = repository_text(entry);
        let runs = text.contains("--no-default-features") || text.contains(&*owner);
        if !runs {
            failures.push(format!(
                "{entry} does not run the isolated feature builds in {owner}"
            ));
        }
    }
}

#[test]
fn integration_examples_compile_with_each_optional_feature() {
    compile_feature_probes();
    let mut failures = Vec::new();
    let reference = repository_text(REFERENCE);
    check_reference_examples(&reference, &mut failures);
    check_support_matrix(&reference, &mut failures);
    check_removed_spellings(&mut failures);
    check_feature_builds(&mut failures);
    assert!(
        failures.is_empty(),
        "{DIAGNOSTIC}: {} disagreements:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
