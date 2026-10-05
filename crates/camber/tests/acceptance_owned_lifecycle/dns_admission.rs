//! 10.T2: DNS preparation starts only after runtime admission, on the direct
//! and the startup path alike, and the admitted owner keeps the provider until
//! its entry settles.
//!
//! Every row enters through public `camber::dns01` provisioning or
//! `RuntimeBuilder` startup. A Cloudflare-shaped local peer records each
//! request the real provider sends, so a refusal row counts zero requests on
//! the wire. The ACME directory is a loopback peer that cannot complete a TLS
//! handshake, so an order that passes preparation ends there with no TXT
//! record. A row that settles an order measures the report budget afterwards
//! by filling it through the real admission path and giving every account
//! back. Each row owns its runtime and peers and returns its own verdict.
#![cfg(feature = "dns01")]

use crate::common::block_on_detached;
use crate::dns_cleanup_peers::{hold_budget, mark_served, release_budget, report_headroom, signal};
use crate::integration_rows::{
    LIVE_LIMIT, REPORT_BUDGET, Row, all, await_signal, bounded, busy, clean_run, expect, expect_eq,
    expect_no_runtime, expect_refused, expect_scope_closed, hold_live_slots,
    integration_admitted_after, invalid_config, permission_denied, refusal, refused, run_rows,
    tempdir,
};
use crate::scripted_peer::lock;
use camber::dns01::{AcmeDns01, CloudflareProvider, DnsProvider, RecordId};
use camber::runtime_test_support::IntegrationLifecycleProbe;
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Resource, RuntimeError, runtime,
};
use serde_json::json;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The hang guard every bounded wait runs under; never a timing assertion.
const BOUND: Duration = Duration::from_secs(10);

/// The configured domains, spread over two zones.
const DOMAINS: [&str; 3] = ["app.example.com", "*.example.com", "www.example.org"];

/// A Cloudflare-shaped peer that knows two zones, and a plain-HTTP peer that
/// stands where the HTTPS ACME directory should be.
struct Peers {
    cloudflare: MockServer,
    directory: MockServer,
}

impl Peers {
    fn start() -> Self {
        block_on_detached(async {
            let cloudflare = MockServer::start().await;
            for (query, zones) in [
                ("app.example.com", json!([])),
                (
                    "example.com",
                    json!([{"id": "zone-com", "name": "example.com"}]),
                ),
                ("www.example.org", json!([])),
                (
                    "example.org",
                    json!([{"id": "zone-org", "name": "example.org"}]),
                ),
            ] {
                Mock::given(method("GET"))
                    .and(path("/zones"))
                    .and(query_param("name", query))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "success": true, "result": zones, "errors": []
                    })))
                    .mount(&cloudflare)
                    .await;
            }
            Self {
                cloudflare,
                directory: MockServer::start().await,
            }
        })
    }

    fn provider(&self) -> Result<CloudflareProvider, String> {
        CloudflareProvider::with_base_url("token".into(), self.cloudflare.uri().into())
            .map_err(|error| format!("descriptor: {error:?}"))
    }

    fn directory_url(&self) -> String {
        format!(
            "{}/directory",
            self.directory.uri().replace("http://", "https://")
        )
    }

    /// Every request the provider sent, as method and path with query.
    fn requests(&self) -> Result<Vec<String>, String> {
        let received = block_on_detached(self.cloudflare.received_requests())
            .ok_or("the Cloudflare peer records no requests")?;
        Ok(received
            .iter()
            .map(|request| {
                let query = request
                    .url
                    .query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default();
                format!("{} {}{query}", request.method, request.url.path())
            })
            .collect())
    }

    fn writes(&self) -> Result<usize, String> {
        Ok(self
            .requests()?
            .iter()
            .filter(|request| !request.starts_with("GET "))
            .count())
    }

    /// The lookups a complete preparation of [`DOMAINS`] sends, in order.
    fn every_zone() -> Vec<String> {
        [
            "app.example.com",
            "example.com",
            "example.com",
            "www.example.org",
            "example.org",
        ]
        .iter()
        .map(|name| format!("GET /zones?name={name}"))
        .collect()
    }
}

/// A DNS-01 configuration of [`DOMAINS`] over a fresh cache in `root`.
fn acme(root: &TempDir, peers: &Peers) -> Result<AcmeDns01, String> {
    AcmeDns01::new("camber", DOMAINS)
        .email("admin@example.com")
        .cache_dir(root.path().join("cache"))
        .directory_url(&peers.directory_url())
        .map_err(|error| format!("directory: {error:?}"))
}

/// Provision `provider` under `acme` on the calling runtime.
fn provision<P: DnsProvider + 'static>(
    acme: &AcmeDns01,
    provider: P,
) -> Result<Result<(), RuntimeError>, String> {
    bounded("provision_cert", BOUND, acme.provision_cert(provider)).map(|outcome| outcome.map(drop))
}

fn expect_invalid(what: &str, outcome: Result<(), RuntimeError>) -> Row {
    match outcome {
        Err(RuntimeError::Config(_)) => Ok(()),
        other => expect_refused(what, other, invalid_config(IntegrationOperation::Provision)),
    }
}

/// Invalid names and bounds refuse before any request.
fn invalid_configuration_refuses_before_io() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let bad_name = AcmeDns01::new("camber", ["exa mple.com"]).cache_dir(root.path());
    let zero = acme(&root, &peers)?.operation_timeout(Duration::ZERO);
    let long = acme(&root, &peers)?.cleanup_timeout(Duration::from_secs(25 * 60 * 60));
    let provider = || peers.provider();
    let (a, b, c) = (provider()?, provider()?, provider()?);
    let outcome = runtime::builder().run(move || -> Row {
        all([
            expect_invalid("an invalid name", provision(&bad_name, a)?),
            expect_invalid("a zero order bound", provision(&zero, b)?),
            expect_invalid("a cleanup bound over 24 hours", provision(&long, c)?),
        ])
    });
    all([
        clean_run(outcome),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// Outside every Camber runtime, provisioning is `NoRuntime` before I/O.
fn no_runtime_refuses_before_io() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let acme = acme(&root, &peers)?;
    let provider = peers.provider()?;
    let outcome = block_on_detached(acme.provision_cert(provider));
    all([
        expect_no_runtime("provisioning outside a runtime", outcome),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// After root admission closed, provisioning is `ScopeClosed` before I/O.
fn closed_admission_refuses_before_io() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let acme = acme(&root, &peers)?;
    let provider = peers.provider()?;
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_scope_closed("provisioning after closure", provision(&acme, provider)?)
    });
    all([
        clean_run(outcome),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// With every live integration slot taken, provisioning is `Busy` before I/O.
fn full_registry_refuses_before_io() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let acme = acme(&root, &peers)?;
    let provider = peers.provider()?;
    let outcome = runtime::builder().run(move || -> Row {
        let held = hold_live_slots(LIVE_LIMIT, IntegrationKind::Nats)?;
        let answer = provision(&acme, provider)?;
        drop(held);
        expect_refused(
            "provisioning at the live limit",
            answer,
            busy(IntegrationOperation::Provision),
        )
    });
    all([
        clean_run(outcome),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// Every report account returns: the whole budget is free again.
///
/// A refused headroom probe counts zero, so the poll also waits out live
/// slots still settling.
fn expect_report_baseline() -> Row {
    bounded(
        "report accounts to return to their baseline",
        BOUND,
        async {
            loop {
                if report_headroom().await? == REPORT_BUDGET {
                    return Ok(());
                }
                tokio::task::yield_now().await;
            }
        },
    )?
}

/// With one report account left, the owner is admitted on it and its order
/// is `Busy` before I/O; every account returns afterwards.
fn full_report_budget_refuses_before_io() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let acme = acme(&root, &peers)?;
    let provider = peers.provider()?;
    let outcome = runtime::builder().run(move || -> Row {
        expect_report_baseline()?;
        // Leave exactly one account: the one the DNS owner's admission takes.
        let budget = hold_budget(1)?;
        let answer = provision(&acme, provider)?;
        let released = release_budget(budget);
        all([
            expect_refused(
                "provisioning with one report account left",
                answer,
                busy(IntegrationOperation::Provision),
            ),
            released,
            expect_report_baseline(),
        ])
    });
    all([
        clean_run(outcome),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// An admitted direct order queries every configured zone, then stops at the
/// directory with no TXT record written.
fn direct_order_prepares_every_zone_after_admission() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let acme = acme(&root, &peers)?;
    let provider = peers.provider()?;
    let outcome = runtime::builder().run(move || -> Row {
        let provisioned = provision(&acme, provider)?;
        expect_eq(
            "the order past preparation",
            refused(provisioned).map(|(operation, ..)| operation),
            Some(IntegrationOperation::Provision),
        )
    });
    all([
        clean_run(outcome),
        expect_eq("zone queries", peers.requests()?, Peers::every_zone()),
        expect_eq("writes sent", peers.writes()?, 0),
    ])
}

/// A failed preparation is the caller's error, writes nothing, and releases
/// its reservations: every report account returns and the runtime ends clean.
fn failed_preparation_writes_nothing() -> Row {
    let server = block_on_detached(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false, "result": null, "errors": [{"code": 9109}]
            })))
            .mount(&server)
            .await;
        server
    });
    let peers = Peers {
        cloudflare: server,
        directory: block_on_detached(MockServer::start()),
    };
    let root = tempdir()?;
    let acme = acme(&root, &peers)?;
    let provider = peers.provider()?;
    let outcome = runtime::builder().run(move || -> Row {
        all([
            expect_refused(
                "the refused preparation",
                provision(&acme, provider)?,
                permission_denied(IntegrationOperation::ZoneLookup),
            ),
            expect_report_baseline(),
        ])
    });
    all([
        clean_run(outcome),
        expect_eq("writes sent", peers.writes()?, 0),
    ])
}

/// Dropping the waiter and the original configuration while preparation is
/// held cancels preparation; the owner keeps the provider until it lets go
/// inside its own settlement, writes nothing, and releases its reservations:
/// every report account returns.
fn waiter_drop_keeps_provider_until_settlement() -> Row {
    let root = tempdir()?;
    let peers = Peers::start();
    let acme = acme(&root, &peers)?;
    let (entered, entered_rx) = oneshot::channel();
    let (dropped, dropped_rx) = oneshot::channel();
    let held = Arc::new(Held {
        entered: Mutex::new(Some(entered)),
        dropped: Mutex::new(Some(dropped)),
        ..Held::default()
    });
    let provider = HeldProvider {
        held: Arc::clone(&held),
    };
    let outcome = runtime::builder().run(move || -> Row {
        let others = hold_live_slots(LIVE_LIMIT - 1, IntegrationKind::Nats)?;
        let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
        await_signal("preparation to begin", BOUND, entered_rx)?;
        let while_held = IntegrationLifecycleProbe::admit(IntegrationKind::Nats).map(drop);
        waiter.cancel();
        drop(waiter);
        await_signal("the provider to be dropped", BOUND, dropped_rx)?;
        let settled = runtime::block_on(integration_admitted_after(
            IntegrationOperation::Connect,
            BOUND,
            || std::future::ready(IntegrationLifecycleProbe::admit(IntegrationKind::Nats)),
        ));
        drop(others);
        all([
            expect_eq(
                "admission while preparation was held",
                refused(while_held).map(|(_, failure, _)| failure),
                Some(IntegrationFailure::Busy),
            ),
            settled,
            expect_report_baseline(),
        ])
    });
    let admission_at_drop = lock(&held.admission_at_drop).clone();
    all([
        clean_run(outcome),
        expect(
            "the preparation future was dropped unfinished",
            held.prepare_dropped.load(Ordering::SeqCst),
        ),
        expect_eq(
            "the live slot when the provider dropped",
            admission_at_drop,
            Some(Err(format!(
                "{:?}",
                Some(busy(IntegrationOperation::Connect))
            ))),
        ),
        expect_eq("provider writes", held.writes.load(Ordering::SeqCst), 0),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// A startup configuration that fails validation refuses before any request
/// and before the closure serves.
fn startup_refuses_invalid_configuration_before_io() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let bad_name = AcmeDns01::new("camber", ["exa mple.com"]).cache_dir(root.path());
    let served = Arc::new(AtomicBool::new(false));
    let names = runtime::builder()
        .tls_auto_dns01(bad_name, "token".into())
        .with_test_dns_transport(&peers.cloudflare.uri())
        .run(mark_served(&served));
    let transport = runtime::builder()
        .tls_auto_dns01(acme(&root, &peers)?, "token".into())
        .with_test_dns_transport("http://provider.example.com")
        .run(mark_served(&served));
    all([
        expect_invalid("an invalid startup name", names),
        expect_refused(
            "a remote plain-HTTP provider",
            transport,
            invalid_config(IntegrationOperation::ZoneLookup),
        ),
        expect("the closure never served", !served.load(Ordering::SeqCst)),
        expect_eq("requests sent", peers.requests()?, Vec::<String>::new()),
    ])
}

/// A registered resource that records each callback the runtime gives it.
#[derive(Clone, Default)]
struct TeardownWitness {
    /// Readiness and health passes that reached it.
    checked: Arc<AtomicUsize>,
    /// Whether teardown shut it down.
    shut_down: Arc<AtomicBool>,
}

impl Resource for TeardownWitness {
    fn name(&self) -> &str {
        "dns-startup-witness"
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        self.checked.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        self.shut_down.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// Startup admits its DNS owner inside the established runtime, queries every
/// configured zone, and a failed first order ends the run through its normal
/// teardown before the closure serves: a registered resource is never probed
/// for readiness, yet is shut down before `run` returns.
fn startup_prepares_every_zone_and_fails_before_serving() -> Row {
    let peers = Peers::start();
    let root = tempdir()?;
    let served = Arc::new(AtomicBool::new(false));
    let witness = TeardownWitness::default();
    let outcome = runtime::builder()
        .resource(witness.clone())
        .tls_auto_dns01(acme(&root, &peers)?, "token".into())
        .with_test_dns_transport(&peers.cloudflare.uri())
        .run(mark_served(&served));
    all([
        expect_eq(
            "the failed first order",
            refused(outcome).map(|(operation, ..)| operation),
            Some(IntegrationOperation::Provision),
        ),
        expect("the closure never served", !served.load(Ordering::SeqCst)),
        expect_eq(
            "readiness passes before the failed order",
            witness.checked.load(Ordering::SeqCst),
            0,
        ),
        expect(
            "teardown shut the resource down before run returned",
            witness.shut_down.load(Ordering::SeqCst),
        ),
        expect_eq("zone queries", peers.requests()?, Peers::every_zone()),
        expect_eq("writes sent", peers.writes()?, 0),
    ])
}

#[test]
fn dns_direct_and_startup_prepare_only_after_runtime_admission() {
    run_rows(&[
        (
            "invalid configuration refuses before I/O",
            invalid_configuration_refuses_before_io,
        ),
        (
            "no runtime refuses before I/O",
            no_runtime_refuses_before_io,
        ),
        (
            "closed admission refuses before I/O",
            closed_admission_refuses_before_io,
        ),
        (
            "a full registry refuses before I/O",
            full_registry_refuses_before_io,
        ),
        (
            "a full report budget refuses before I/O",
            full_report_budget_refuses_before_io,
        ),
        (
            "a direct order prepares every zone after admission",
            direct_order_prepares_every_zone_after_admission,
        ),
        (
            "a failed preparation writes nothing",
            failed_preparation_writes_nothing,
        ),
        (
            "a waiter drop keeps the provider until settlement",
            waiter_drop_keeps_provider_until_settlement,
        ),
        (
            "startup refuses invalid configuration before I/O",
            startup_refuses_invalid_configuration_before_io,
        ),
        (
            "startup prepares every zone and fails before serving",
            startup_prepares_every_zone_and_fails_before_serving,
        ),
    ]);
}

/// What the held provider observed.
#[derive(Default)]
struct Held {
    /// Whether preparation began.
    entered: Mutex<Option<oneshot::Sender<()>>>,
    /// Whether the preparation future was dropped before it finished.
    prepare_dropped: AtomicBool,
    /// Record writes the provider was asked for.
    writes: AtomicUsize,
    /// The live-slot admission the provider's drop observed.
    admission_at_drop: Mutex<Option<Result<(), String>>>,
    /// Signalled once the provider is dropped.
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}

/// A provider whose preparation never finishes by itself.
struct HeldProvider {
    held: Arc<Held>,
}

/// Records that preparation was dropped unfinished.
struct PrepareDrop(Arc<Held>);

impl Drop for PrepareDrop {
    fn drop(&mut self) {
        self.0.prepare_dropped.store(true, Ordering::SeqCst);
    }
}

impl DnsProvider for HeldProvider {
    fn prepare(&mut self, _: &[Arc<str>]) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        let held = Arc::clone(&self.held);
        async move {
            let _dropped = PrepareDrop(Arc::clone(&held));
            signal(&held.entered);
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    fn create_txt_record(
        &self,
        _: &str,
        _: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        self.held.writes.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(RecordId::from("held")))
    }

    fn delete_txt_record(&self, _: &str) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.held.writes.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(()))
    }
}

impl Drop for HeldProvider {
    fn drop(&mut self) {
        // The provider drops inside its owner's task. A live slot still taken
        // by that owner means the owner had not settled when it let go.
        let admission = IntegrationLifecycleProbe::admit(IntegrationKind::Nats)
            .map(drop)
            .map_err(|error| format!("{:?}", refusal(&error)));
        *lock(&self.held.admission_at_drop) = Some(admission);
        signal(&self.held.dropped);
    }
}
