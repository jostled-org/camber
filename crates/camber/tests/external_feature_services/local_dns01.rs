#![cfg(feature = "dns01")]
//! 20.T1–T3: DNS-01 issuance, serving, renewal, and cancellation against
//! Pebble and challtestsrv.
//!
//! Selected through `.github/scripts/check-local-integrations.sh dns`, which
//! owns both service containers and publishes their loopback addresses and
//! Pebble's own TLS root. Every request Camber sends goes to loopback with a
//! dummy token; no test reads a cloud credential.
//!
//! A Cloudflare-shaped peer the row owns stands in for the provider API. It
//! mirrors each record it commits or deletes into challtestsrv before it
//! answers, so Pebble validates against exactly the records the peer's zones
//! hold. The peer proves Camber's provider requests, not Cloudflare's
//! production behavior. Each zone holds an unrelated TXT record under a
//! challenge name the order also uses.
//!
//! The oracle reads the peer's store, the DNS server Pebble reads, the cache,
//! and what a hostname-verifying TLS client is served.

use crate::challenge_dns::{ChallengeServer, address, environment};
use crate::dns_cleanup_peers::{
    CfLog, DeleteAnswer, MirroredPeer, SENTINEL_ID, SENTINEL_NAME, SENTINEL_VALUE, Script, Stage,
    TWO_ZONES, await_renewal_waits, cached_bundle, chain_leaf, challenge_name,
    directory_configuration, dns_accounts, elapse_renewal, expect_delivered, expect_exact_deletes,
    expect_retained, expected_unresolved, generation_expiring, integration, leaf_of, run_observing,
    seed_cache, zone_id_of,
};
use crate::integration_rows::{Row, all, bounded, expect, expect_eq, observed_verdict};
use crate::local_readiness::Readiness;
use crate::resources::selected_run;
use camber::dns01::AcmeDns01;
use camber::http::{Request, Response, Router};
use camber::runtime_test_support::runtime_schedule;
use camber::{RuntimeBuilder, RuntimeError, runtime};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::future::Future;
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The hang guard on one bounded step; never a timing assertion.
const STEP_BOUND: Duration = Duration::from_secs(30);

/// The hang guard on one whole order against Pebble.
const ORDER_BOUND: Duration = Duration::from_secs(120);

/// The shared bound on both services' readiness acknowledgements.
const READINESS_BOUND: Duration = Duration::from_secs(60);

/// The bytes a served page may answer with.
const MAX_PAGE_BYTES: u64 = 64 * 1024;

/// Days left on a cached leaf that is due for renewal.
const DUE_DAYS: u32 = 20;

/// The token every provider request carries. It names no account.
const DUMMY_TOKEN: &str = "camber-local-token";

/// Three domains in two zones; one is two labels below its zone.
const MULTI_ZONE: [&str; 3] = ["app.example.com", "deep.app.example.com", "www.example.org"];

// --- the services ----------------------------------------------------------

/// Pebble and challtestsrv as the lane runner published them, ready.
struct LocalAcme {
    /// The root Pebble's own HTTPS listeners present.
    trust: Box<[u8]>,
    /// Pebble's ACME directory.
    directory: Box<str>,
    /// The root Pebble issues certificates under.
    issuing_root: CertificateDer<'static>,
    challenge: Arc<ChallengeServer>,
    /// Drives the fixture's own requests, outside every Camber runtime.
    driver: tokio::runtime::Runtime,
}

impl LocalAcme {
    /// Read the published endpoints, wait for both services' protocol
    /// acknowledgements, and read Pebble's issuing root.
    fn start() -> Result<Self, String> {
        let trust: Box<[u8]> = std::fs::read(environment("CAMBER_LOCAL_PEBBLE_TRUST")?)
            .map_err(|error| format!("read Pebble's TLS root: {error}"))?
            .into_boxed_slice();
        let acme = address("CAMBER_LOCAL_PEBBLE_ACME")?;
        let management = address("CAMBER_LOCAL_PEBBLE_MANAGEMENT")?;
        let challenge = Arc::new(ChallengeServer::from_environment()?);
        await_ready("pebble", acme, &Readiness::acme_directory(&trust))?;
        await_ready(
            "challtestsrv",
            challenge.control(),
            &Readiness::challenge_control(),
        )?;
        let driver = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("fixture runtime: {error}"))?;
        let issuing_root = drive(
            &driver,
            "Pebble's issuing root",
            issuing_root(&trust, management),
        )??;
        Ok(Self {
            trust,
            directory: format!("https://localhost:{}/dir", acme.port()).into_boxed_str(),
            issuing_root,
            challenge,
            driver,
        })
    }

    /// Run `row` over a fresh zone under `create` and `delete`, then tear the
    /// zone down whatever `row` found.
    fn in_zone(
        &self,
        create: Script,
        delete: Script,
        row: impl FnOnce(&Self, &LocalZone) -> Row,
    ) -> Row {
        let zone = self.zone(create, delete)?;
        let checks = row(self, &zone);
        all([checks, zone.finish()])
    }

    /// A fresh zone peer and cache under `create` and `delete`.
    fn zone(&self, create: Script, delete: Script) -> Result<LocalZone, String> {
        let root = TempDir::new().map_err(|error| format!("cache root: {error}"))?;
        let cache = root.path().join("cache");
        let peer = MirroredPeer::start(Arc::clone(&self.challenge), create, delete, &cache)?;
        Ok(LocalZone { peer, cache, root })
    }

    /// A DNS-01 configuration of `domains` over `zone`'s cache, ordering from
    /// Pebble and trusting only its root beside the platform's.
    fn configuration(&self, zone: &LocalZone, domains: &[&str]) -> Result<AcmeDns01, String> {
        directory_configuration(&zone.cache, domains, &self.directory, &self.trust)
    }

    /// A runtime whose DNS-01 startup orders `domains` from Pebble through
    /// `zone`'s peer.
    fn startup(
        &self,
        builder: RuntimeBuilder,
        zone: &LocalZone,
        domains: &[&str],
    ) -> Result<RuntimeBuilder, String> {
        Ok(builder
            .tls_auto_dns01(self.configuration(zone, domains)?, DUMMY_TOKEN.into())
            .with_test_dns_transport(&zone.peer.cloudflare.uri()))
    }

    /// Every TXT value the DNS server Pebble reads answers for `name`.
    fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        drive(&self.driver, "a TXT query", self.challenge.txt(name))?
    }

    /// DNS answers exactly the values the peer's zones hold at each of
    /// `domains`' challenge names, and the unrelated record among them.
    fn expect_dns(&self, log: &CfLog, domains: &[&str]) -> Row {
        let mirrored = challenge_names(domains).into_iter().map(|name| {
            let mut stored: Vec<String> = log
                .records
                .values()
                .filter(|record| *record.name == *name)
                .map(|record| record.content.to_string())
                .collect();
            stored.sort();
            expect_eq(
                &format!("TXT values DNS answers at {name}"),
                self.txt(&name)?,
                stored,
            )
        });
        let sentinel = self.txt(SENTINEL_NAME).and_then(|values| {
            expect(
                "DNS lost the unrelated record",
                values.iter().any(|value| value == SENTINEL_VALUE),
            )
        });
        all(mirrored.chain([sentinel]))
    }

    /// `chain` is valid under Pebble's root for every one of `domains`.
    fn expect_issued(&self, chain: &[CertificateDer<'static>], domains: &[&str]) -> Row {
        let verifier = server_verifier(&self.issuing_root)?;
        let Some((leaf, intermediates)) = chain.split_first() else {
            return Err("the issued chain is empty".to_owned());
        };
        all(domains.iter().map(|domain| {
            verifier
                .verify_server_cert(
                    leaf,
                    intermediates,
                    &server_name(domain)?,
                    &[],
                    UnixTime::now(),
                )
                .map(drop)
                .map_err(|error| format!("the issued chain for {domain}: {error}"))
        }))
    }

    /// Prove the DNS server answers nothing at any of `domains`' challenge
    /// names, after every zone tore down, and name them for the witness.
    fn finish(self, domains: &[&str]) -> Result<Box<[String]>, String> {
        let names = challenge_names(domains);
        all(names.iter().map(|name| {
            expect_eq(
                &format!("TXT values left at {name}"),
                self.txt(name)?,
                Vec::new(),
            )
        }))?;
        Ok(names)
    }
}

/// One row's zone peer and the cache its orders publish into.
struct LocalZone {
    peer: MirroredPeer<ChallengeServer>,
    cache: PathBuf,
    root: TempDir,
}

impl LocalZone {
    fn log(&self) -> CfLog {
        self.peer.cloudflare.log()
    }

    fn bundle(&self) -> Option<Vec<u8>> {
        cached_bundle(&self.cache)
    }

    /// Remove every record the peer mirrored, stop it, and remove the cache.
    fn finish(self) -> Row {
        let Self { peer, root, .. } = self;
        all([
            peer.finish(),
            root.close()
                .map_err(|error| format!("remove the cache root: {error}")),
        ])
    }
}

/// Run `rows` against the lane's services, tear them down, and emit the
/// cleanup witness for `domains`' challenge names.
///
/// The witness is written once teardown is proven, even when a row panicked;
/// the panic then continues, and a failed row fails the test. A failed
/// teardown is reported beside the rows' own failure, never in place of it.
fn run_local(domains: &[&str], rows: impl FnOnce(&LocalAcme) -> Row) {
    let (run, witness) = selected_run().expect("a valid external run ID and witness path");
    let local = LocalAcme::start().expect("the local ACME services are ready");
    let verdict = std::panic::catch_unwind(AssertUnwindSafe(|| rows(&local)));
    let cleaned = local.finish(domains).map(|names| {
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        witness
            .emit(&run, &names)
            .expect("emit the cleanup witness after every zone tore down");
    });
    match (verdict, cleaned) {
        (Ok(Ok(())), Ok(())) => {}
        (Ok(Ok(())), Err(kept)) => panic!("the local services kept records: {kept}"),
        (Ok(Err(reason)), Ok(())) => panic!("rows failed:\n{reason}"),
        (Ok(Err(reason)), Err(kept)) => {
            panic!("rows failed:\n{reason}\nthe local services kept records: {kept}")
        }
        (Err(panic), Ok(())) => std::panic::resume_unwind(panic),
        (Err(panic), Err(kept)) => {
            eprintln!("the local services kept records: {kept}");
            std::panic::resume_unwind(panic)
        }
    }
}

// --- the shared oracle -------------------------------------------------------

/// Each domain's challenge name.
fn challenge_names(domains: &[&str]) -> Box<[String]> {
    domains
        .iter()
        .map(|domain| challenge_name(domain))
        .collect()
}

/// Preparation looked up every domain's zone, walking each name up to its
/// zone, before any record was written.
fn expect_zone_lookups(requests: &[Box<str>], domains: &[&str]) -> Row {
    let mut expected: Vec<String> = domains
        .iter()
        .flat_map(|domain| {
            std::iter::successors(Some(*domain), |name| name.split_once('.').map(|(_, up)| up))
                .filter(|name| name.contains('.'))
                .map(|name| format!("GET /zones?name={name}"))
        })
        .collect();
    expected.sort();
    let mut lookups: Vec<String> = requests
        .iter()
        .filter(|request| request.starts_with("GET /zones"))
        .map(ToString::to_string)
        .collect();
    lookups.sort();
    let last_lookup = requests
        .iter()
        .rposition(|request| request.starts_with("GET /zones"));
    let first_write = requests
        .iter()
        .position(|request| !request.starts_with("GET /zones"));
    all([
        expect_eq("zone lookups", lookups, expected),
        expect(
            "a record was written before every zone was prepared",
            first_write.is_none_or(|first| last_lookup.is_some_and(|last| last < first)),
        ),
    ])
}

/// Exactly one acknowledged create per domain, each sent to its own zone.
fn expect_creates(log: &CfLog, domains: &[&str]) -> Row {
    let mut created: Vec<&str> = log.creates.iter().map(|create| &*create.name).collect();
    created.sort_unstable();
    let mut names = challenge_names(domains).into_vec();
    names.sort();
    let mut writes: Vec<&str> = log
        .requests
        .iter()
        .map(|request| &**request)
        .filter(|request| request.starts_with("POST "))
        .collect();
    writes.sort();
    let mut zones: Vec<String> = domains
        .iter()
        .map(|domain| {
            zone_id_of(domain)
                .map(|zone| format!("POST /zones/{zone}/dns_records"))
                .ok_or_else(|| format!("{domain} is in no zone the peer knows"))
        })
        .collect::<Result<_, _>>()?;
    zones.sort();
    all([
        expect_eq(
            "created challenge names",
            created,
            names.iter().map(String::as_str).collect(),
        ),
        expect_eq(
            "creates by zone",
            writes,
            zones.iter().map(String::as_str).collect(),
        ),
        expect_eq(
            "acknowledged creates",
            log.acknowledged().len(),
            domains.len(),
        ),
    ])
}

/// Every acknowledged record was deleted by its exact ID, once, and nothing
/// else was deleted.
fn expect_every_record_deleted(log: &CfLog) -> Row {
    all(log
        .acknowledged()
        .iter()
        .filter_map(|create| create.id.as_deref())
        .map(|id| {
            expect_eq(
                &format!("deletes of record {id}"),
                log.deletes
                    .iter()
                    .filter(|delete| &*delete.id == id && delete.answer == DeleteAnswer::Deleted)
                    .count(),
                1,
            )
        })
        .chain([expect_eq(
            "deletes",
            log.deletes.len(),
            log.acknowledged().len(),
        )]))
}

/// The zones hold the unrelated record and nothing else, and the mirror
/// refused nothing.
fn expect_only_sentinel(log: &CfLog) -> Row {
    all([
        expect_eq(
            "records the zones hold",
            log.records.keys().map(|id| &**id).collect::<Vec<_>>(),
            vec![SENTINEL_ID],
        ),
        expect_no_mirror_failures(log),
    ])
}

/// The mirror refused nothing.
fn expect_no_mirror_failures(log: &CfLog) -> Row {
    expect_eq(
        "mirror failures",
        log.mirror_failures.as_slice(),
        &[] as &[String],
    )
}

/// Every delete in `log` arrived while the cache held `cached`: cleanup
/// settled before the order published.
fn expect_cleanup_before_publication(log: &CfLog, cached: Option<&[u8]>) -> Row {
    all(log.deletes.iter().map(|delete| {
        expect_eq(
            &format!("the cache when record {} was deleted", delete.id),
            delete.cached.as_deref(),
            cached,
        )
    }))
}

/// Prepare every domain, write only challenged domains, and delete those records
/// before publication. Keep unrelated DNS records intact.
fn expect_settled_order(
    local: &LocalAcme,
    log: &CfLog,
    domains: &[&str],
    challenged: &[&str],
    cached: Option<&[u8]>,
) -> Row {
    all([
        expect_zone_lookups(&log.requests, domains),
        expect_creates(log, challenged),
        expect_every_record_deleted(log),
        expect_cleanup_before_publication(log, cached),
        expect_exact_deletes(log, &[]),
        expect_only_sentinel(log),
        local.expect_dns(log, domains),
    ])
}

// --- 20.T1 -------------------------------------------------------------------

/// Direct provisioning prepares every zone, writes and deletes exactly its
/// own records, and returns a validated chain it published.
fn multizone_issuance(local: &LocalAcme) -> Row {
    local.in_zone(Script::answer(), Script::answer(), issue_directly)
}

fn issue_directly(local: &LocalAcme, zone: &LocalZone) -> Row {
    let acme = local.configuration(zone, &MULTI_ZONE)?;
    let provider = zone.peer.provider()?;
    let outcome = runtime::builder().run(move || {
        bounded(
            "direct provisioning",
            ORDER_BOUND,
            acme.provision_cert(provider),
        )
    });
    let key = match outcome {
        Ok(Ok(Ok(key))) => key,
        other => return Err(format!("direct provisioning answered {other:?}")),
    };
    let log = zone.log();
    all([
        expect_settled_order(local, &log, &MULTI_ZONE, &MULTI_ZONE, None),
        local.expect_issued(&key.cert, &MULTI_ZONE),
        expect_eq(
            "the cached leaf",
            zone.bundle().and_then(|bundle| leaf_of(&bundle)),
            chain_leaf(&key.cert),
        ),
    ])
}

#[test]
#[ignore = "external lane dns; owner: Camber ACME and DNS integrations; run: .github/scripts/check-local-integrations.sh dns"]
fn dns_local_acme_multizone_issuance_and_cleanup() {
    run_local(&MULTI_ZONE, multizone_issuance);
}

// --- 20.T2 -------------------------------------------------------------------

/// Serve over TLS on the calling runtime, and read the leaf a client that
/// verifies `root` and each of `domains` as its hostname is served.
fn served_leaves(
    root: &CertificateDer<'static>,
    domains: &[&str],
) -> Result<Box<[Vec<u8>]>, String> {
    let config = client_config(root)?;
    bounded("TLS serving", STEP_BOUND, async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("bind: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("local address: {error}"))?;
        let mut router = Router::new();
        router.get("/", |_request: &Request| async {
            Response::text(200, "served")
        });
        let server = camber::http::serve_background(listener, router)
            .map_err(|error| format!("serve: {error:?}"))?;
        let mut leaves = Vec::with_capacity(domains.len());
        for domain in domains {
            leaves.push(fetch_leaf(&config, address, domain).await);
        }
        let stopped = server.shutdown_and_join().await;
        let leaves = leaves.into_iter().collect::<Result<Box<[_]>, _>>()?;
        stopped.map_err(|error| format!("the server stopped with {error:?}"))?;
        Ok(leaves)
    })?
}

/// Connect to `address` as `domain`, require the page, and answer the leaf.
async fn fetch_leaf(
    config: &Arc<rustls::ClientConfig>,
    address: SocketAddr,
    domain: &str,
) -> Result<Vec<u8>, String> {
    let name = server_name(domain)?;
    let tcp = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|error| format!("connect as {domain}: {error}"))?;
    let mut tls = tokio_rustls::TlsConnector::from(Arc::clone(config))
        .connect(name, tcp)
        .await
        .map_err(|error| format!("the handshake as {domain}: {error}"))?;
    let served = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(chain_leaf)
        .ok_or_else(|| format!("{domain} was served no certificate"))?;
    tls.write_all(
        format!("GET / HTTP/1.1\r\nHost: {domain}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .await
    .map_err(|error| format!("request as {domain}: {error}"))?;
    let mut page = Vec::new();
    match (&mut tls).take(MAX_PAGE_BYTES).read_to_end(&mut page).await {
        // A peer that closes without `close_notify` still delivered its page.
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
        Err(error) => return Err(format!("read the page as {domain}: {error}")),
    }
    let page = String::from_utf8_lossy(&page);
    match page.starts_with("HTTP/1.1 200") && page.ends_with("served") {
        true => Ok(served),
        false => Err(format!("{domain} was answered {page:?}")),
    }
}

/// Startup issues and serves; a restart serves `certificate.pem` without an
/// order; a renewal due by its cached leaf's validity serves its replacement
/// only after its cleanup settled.
fn startup_restart_and_renewal(local: &LocalAcme) -> Row {
    all([true, false].into_iter().map(|reuse| {
        local
            .in_zone(Script::answer(), Script::answer(), |local, zone| {
                serve_restart_and_renew(local, zone, reuse)
            })
            .map_err(|reason| format!("authorization reuse={reuse}: {reason}"))
    }))
}

/// A new account forces fresh challenges even when Pebble always reuses valid ones.
fn renewal_challenges(zone: &LocalZone, reuse: bool) -> Result<&'static [&'static str], String> {
    match reuse {
        true => Ok(&[]),
        false => {
            std::fs::remove_file(zone.cache.join("account.json"))
                .map_err(|error| format!("remove the fixture account: {error}"))?;
            Ok(&TWO_ZONES)
        }
    }
}

fn serve_restart_and_renew(local: &LocalAcme, zone: &LocalZone, reuse: bool) -> Row {
    let root = &local.issuing_root;
    let started = local
        .startup(runtime::builder(), zone, &TWO_ZONES)?
        .run(|| served_leaves(root, &TWO_ZONES));
    let first = match started {
        Ok(Ok(leaves)) => leaves,
        other => return Err(format!("the first startup answered {other:?}")),
    };
    let issued = zone.log();
    let issued_leaf = zone.bundle().and_then(|bundle| leaf_of(&bundle));
    let startup = all([
        expect_settled_order(local, &issued, &TWO_ZONES, &TWO_ZONES, None),
        expect_eq(
            "leaves served at startup",
            &*first,
            &vec![issued_leaf.clone().unwrap_or_default(); TWO_ZONES.len()][..],
        ),
    ]);

    let due = generation_expiring(&TWO_ZONES, DUE_DAYS)?;
    let challenged = renewal_challenges(zone, reuse)?;
    let controller = runtime_schedule();
    let builder = local.startup(
        runtime::builder().with_test_schedule(&controller),
        zone,
        &TWO_ZONES,
    )?;
    let restarted = builder.run(|| -> Result<Restart, String> {
        await_renewal_waits(&controller, 1, STEP_BOUND)?;
        let restarted = served_leaves(root, &TWO_ZONES)?;
        let requests = zone.log().requests.len();
        seed_cache(&zone.cache, &due)?;
        elapse_renewal(&controller, 1, STEP_BOUND)?;
        await_renewal_waits(&controller, 2, ORDER_BOUND)?;
        Ok(Restart {
            restarted,
            requests,
            renewed: served_leaves(root, &TWO_ZONES)?,
        })
    });
    let Restart {
        restarted,
        requests: requests_at_restart,
        renewed,
    } = match restarted {
        Ok(Ok(observed)) => observed,
        other => return Err(format!("the restart answered {other:?}")),
    };
    let renewal = renewal_log(&issued, &zone.log());
    let renewed_leaf = zone.bundle().and_then(|bundle| leaf_of(&bundle));
    all([
        startup,
        expect_eq("leaves served after the restart", restarted, first),
        expect_eq(
            "provider requests of the restart",
            requests_at_restart,
            issued.requests.len(),
        ),
        expect_settled_order(
            local,
            &renewal,
            &TWO_ZONES,
            challenged,
            Some(due.as_bytes()),
        ),
        expect(
            "the renewal published the leaf it replaced",
            renewed_leaf.is_some() && renewed_leaf != issued_leaf,
        ),
        expect_eq(
            "leaves served after the renewal",
            renewed,
            vec![renewed_leaf.unwrap_or_default(); TWO_ZONES.len()].into_boxed_slice(),
        ),
    ])
}

/// What the restarted runtime observed.
#[derive(Debug)]
struct Restart {
    /// The leaves served before the renewal, one per domain.
    restarted: Box<[Vec<u8>]>,
    /// The provider requests seen once the restart served.
    requests: usize,
    /// The leaves served after the renewal, one per domain.
    renewed: Box<[Vec<u8>]>,
}

/// The entries of `now` past the first `seen`.
fn after<T: Clone>(now: &[T], seen: usize) -> Vec<T> {
    now.get(seen..).unwrap_or_default().to_vec()
}

/// The renewal's own requests and records: everything `now` saw after
/// `before`, over the zones as they are now.
fn renewal_log(before: &CfLog, now: &CfLog) -> CfLog {
    CfLog {
        requests: after(&now.requests, before.requests.len()),
        creates: after(&now.creates, before.creates.len()),
        deletes: after(&now.deletes, before.deletes.len()),
        ..now.clone()
    }
}

#[test]
#[ignore = "external lane dns; owner: Camber ACME and DNS integrations; run: .github/scripts/check-local-integrations.sh dns"]
fn dns_local_startup_renewal_and_cache_restart() {
    run_local(&TWO_ZONES, startup_restart_and_renewal);
}

// --- 20.T3 -------------------------------------------------------------------

/// The peer acknowledges the first create and holds the second unanswered,
/// so the order is stopped with one real record and one of unknown outcome.
fn held_second_create() -> Script {
    Script::answer().nth(2, Stage::Hold)
}

/// Wait until the first record is acknowledged and the second create is held.
fn await_held(zone: &LocalZone) -> Row {
    bounded(
        "a second create to be held after the first was acknowledged",
        ORDER_BOUND,
        zone.peer
            .cloudflare
            .until(|log| log.acknowledged().len() == 1 && log.creates.len() == 2),
    )
}

/// What a stopped order must leave: the acknowledged record deleted by its
/// ID, the held one named by its domain alone, the unrelated record intact,
/// nothing published, and DNS answering exactly the zones' records.
fn expect_stopped_order(
    local: &LocalAcme,
    zone: &LocalZone,
    teardown: &Result<(), RuntimeError>,
) -> Row {
    let log = zone.log();
    let expected = expected_unresolved(&log, &[]);
    let acknowledged = log
        .acknowledged()
        .first()
        .and_then(|create| create.id.clone());
    all([
        expect_eq("records named unresolved", expected.len(), 1),
        expect(
            "the acknowledged record was not deleted",
            acknowledged.as_deref().is_some_and(|id| log.deleted(id)),
        ),
        expect_exact_deletes(&log, &[]),
        expect_retained(&dns_accounts(teardown)?, &expected),
        expect("a stopped order published", zone.bundle().is_none()),
        expect_no_mirror_failures(&log),
        local.expect_dns(&log, &TWO_ZONES),
    ])
}

/// The public waiter is dropped once a record is real; the owner still
/// deletes it and names the held create.
fn dropped_waiter(local: &LocalAcme) -> Row {
    local.in_zone(held_second_create(), Script::answer(), drop_waiter)
}

fn drop_waiter(local: &LocalAcme, zone: &LocalZone) -> Row {
    let acme = local.configuration(zone, &TWO_ZONES)?;
    let provider = zone.peer.provider()?;
    let peer = zone.peer.cloudflare.clone();
    let (observed, teardown) = run_observing(runtime::builder(), || -> Row {
        let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
        await_held(zone)?;
        waiter.cancel();
        drop(waiter);
        let deleted = bounded(
            "the acknowledged record's delete",
            STEP_BOUND,
            peer.until(|log| {
                log.deletes
                    .iter()
                    .any(|delete| delete.answer != DeleteAnswer::Pending)
            }),
        );
        peer.release();
        deleted
    });
    all([
        observed_verdict(observed),
        expect_stopped_order(local, zone, &teardown),
    ])
}

/// The runtime stops once a record is real; the order deletes it and names
/// the held create to its caller, or the caller is cancelled with the stop.
fn stopped_runtime(local: &LocalAcme) -> Row {
    local.in_zone(held_second_create(), Script::answer(), stop_runtime)
}

fn stop_runtime(local: &LocalAcme, zone: &LocalZone) -> Row {
    let acme = local.configuration(zone, &TWO_ZONES)?;
    let provider = zone.peer.provider()?;
    let peer = zone.peer.cloudflare.clone();
    let (observed, teardown) = run_observing(runtime::builder(), || {
        let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
        await_held(zone)?;
        runtime::request_shutdown();
        let answer = bounded(
            "the stopped order's answer",
            STEP_BOUND,
            waiter.into_future(),
        );
        peer.release();
        answer
    });
    let caller = match observed {
        Some(Ok(answer)) => answer.and_then(|answer| answer),
        other => return Err(format!("the stopped order answered {other:?}")),
    };
    let expected = expected_unresolved(&zone.log(), &[]);
    all([
        match &caller {
            Err(RuntimeError::Cancelled) => Ok(()),
            Err(error) => integration(error).map_or_else(
                || Err(format!("the caller was answered {error:?}")),
                |error| expect_delivered(error, &expected),
            ),
            Ok(_) => Err("a stopped order issued".to_owned()),
        },
        expect_stopped_order(local, zone, &teardown),
    ])
}

#[test]
#[ignore = "external lane dns; owner: Camber ACME and DNS integrations; run: .github/scripts/check-local-integrations.sh dns"]
fn dns_local_cancelled_order_keeps_unrelated_txt() {
    run_local(&TWO_ZONES, |local| {
        all([dropped_waiter(local), stopped_runtime(local)])
    });
}

// --- trust ---------------------------------------------------------------------

/// Wait, inside one bound, for `service` to acknowledge its protocol.
fn await_ready(service: &str, address: SocketAddr, readiness: &Readiness) -> Row {
    readiness
        .await_ready(address, Instant::now() + READINESS_BOUND)
        .map_err(|last| format!("{service} at {address} never acknowledged readiness: {last}"))
}

/// Drive `future` on the fixture's runtime under the step bound.
fn drive<F: Future>(
    driver: &tokio::runtime::Runtime,
    what: &str,
    future: F,
) -> Result<F::Output, String> {
    // The timer is built inside `block_on`, where the driver's clock exists.
    driver
        .block_on(async { tokio::time::timeout(STEP_BOUND, future).await })
        .map_err(|_| format!("{what} did not finish within {STEP_BOUND:?}"))
}

/// The root Pebble issues under, read from its management API, which
/// presents the same TLS root as its directory.
async fn issuing_root(
    trust: &[u8],
    management: SocketAddr,
) -> Result<CertificateDer<'static>, String> {
    let certificate = reqwest::Certificate::from_pem(trust)
        .map_err(|error| format!("Pebble's TLS root: {error}"))?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .user_agent("camber-local-dns01")
        .tls_certs_merge([certificate])
        .timeout(STEP_BOUND)
        .build()
        .map_err(|error| format!("management client: {error}"))?;
    let response = client
        .get(format!("https://localhost:{}/roots/0", management.port()))
        .send()
        .await
        .map_err(|error| format!("Pebble's issuing root: {error}"))?;
    let pem = match response.status() {
        reqwest::StatusCode::OK => response
            .bytes()
            .await
            .map_err(|error| format!("Pebble's issuing root: {error}"))?,
        status => return Err(format!("Pebble's issuing root answered {status}")),
    };
    CertificateDer::from_pem_slice(&pem).map_err(|error| format!("Pebble's issuing root: {error}"))
}

/// `domain` as the hostname a TLS client verifies.
fn server_name(domain: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(domain.to_owned()).map_err(|error| format!("{domain}: {error}"))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn roots(root: &CertificateDer<'static>) -> Result<Arc<rustls::RootCertStore>, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(root.clone())
        .map_err(|error| format!("Pebble's issuing root: {error}"))?;
    Ok(Arc::new(roots))
}

/// A verifier that trusts `root` alone.
fn server_verifier(root: &CertificateDer<'static>) -> Result<Arc<WebPkiServerVerifier>, String> {
    WebPkiServerVerifier::builder_with_provider(roots(root)?, provider())
        .build()
        .map_err(|error| format!("verifier: {error}"))
}

/// A TLS client that trusts `root` alone and verifies every hostname.
fn client_config(root: &CertificateDer<'static>) -> Result<Arc<rustls::ClientConfig>, String> {
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("client config: {error}"))?
        .with_root_certificates(roots(root)?)
        .with_no_client_auth();
    Ok(Arc::new(config))
}
