//! 10.T3: configured ACME trust and every request bound reach the real client.
//!
//! A local TLS peer stands in for the ACME directory and records each request
//! that completes a handshake, so a row counts what the actual instant-acme
//! client sent through Camber's transport. A Cloudflare-shaped peer records the
//! provider's requests the same way. Each row owns its peers and runtime, and
//! finishes them on success and failure alike.
#![cfg(feature = "dns01")]

use crate::dns_cleanup_peers::{
    DNS_ROW_BOUND, Fixture, Pki, Script, Scripts, Stage, TWO_ZONES, challenge_name,
    expect_exact_deletes, leaf_of,
};
use crate::integration_rows::{
    Refusal, Row, all, bounded, clean_run, expect, expect_eq, expired, invalid_config,
    is_configuration_refusal, on_tokio, refused, rejected, run_rows, tempdir, unavailable, unknown,
};
use crate::scripted_peer::{accept_until, bind_loopback, lock};
use camber::dns01::{AcmeDns01, CloudflareProvider, DnsProvider, RecordId};
use camber::{IntegrationOperation, RuntimeError, runtime};
use instant_acme::AuthorizationStatus;
use std::future::Future;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The documented cap on one provider or directory request.
const REQUEST_CAP: Duration = Duration::from_secs(10);

/// The file a DNS-01 cache keeps its ACME account credentials in.
const ACCOUNT_FILE: &str = "account.json";

#[test]
fn dns_authorization_states_control_challenge_submission() {
    use AuthorizationStatus::{Deactivated, Expired, Invalid, Pending, Revoked, Valid};
    let states = [
        [Pending, Pending],
        [Valid, Pending],
        [Pending, Valid],
        [Valid, Valid],
        [Pending, Invalid],
        [Pending, Revoked],
        [Pending, Expired],
        [Pending, Deactivated],
    ];
    all(states.into_iter().map(|states| {
        authorization_state_row(states).map_err(|reason| format!("{states:?}: {reason}"))
    }))
    .expect("ACME authorization states must control DNS writes and challenge posts");
}

fn authorization_state_row(states: [AuthorizationStatus; 2]) -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    for (domain, status) in TWO_ZONES.into_iter().zip(states) {
        fixture.peers.acme.authorization_state(domain, status);
    }
    let result = provision(
        fixture.configuration(&TWO_ZONES)?,
        fixture.peers.provider()?,
    )?;
    let log = fixture.peers.cloudflare.log();
    let acme = fixture.peers.acme.log();
    let expected_names: Vec<Box<str>> = TWO_ZONES
        .into_iter()
        .zip(states)
        .filter(|(_, status)| *status == AuthorizationStatus::Pending)
        .map(|(domain, _)| challenge_name(domain).into())
        .collect();
    let succeeds = states.iter().all(|status| {
        matches!(
            status,
            AuthorizationStatus::Pending | AuthorizationStatus::Valid
        )
    });
    let issued = match succeeds {
        true => all([
            expect(
                &format!("issuance failed: {:?}", result.0),
                result.0.is_ok(),
            ),
            expect("the peer issued no certificate", acme.issued.is_some()),
            expect_eq(
                "published leaf",
                fixture.bundle().and_then(|pem| leaf_of(&pem)),
                acme.issued,
            ),
        ]),
        false => all([
            expect_eq(
                "unusable authorization",
                refused(result.0),
                Some(rejected(IntegrationOperation::Provision)),
            ),
            expect(
                "a refused order published a certificate",
                fixture.bundle().is_none(),
            ),
            expect("a refused order finalized", acme.issued.is_none()),
        ]),
    };
    let checks = all([
        issued,
        expect_eq("challenge posts", acme.challenges, expected_names.len()),
        expect_eq(
            "created names",
            log.creates
                .iter()
                .map(|record| record.name.clone())
                .collect(),
            expected_names,
        ),
        expect(
            "created records were not deleted",
            log.acknowledged()
                .iter()
                .all(|record| record.id.as_deref().is_some_and(|id| log.deleted(id))),
        ),
        expect_exact_deletes(&log, &[]),
    ]);
    all([checks, fixture.finish()])
}

/// An order bound shorter than the request cap.
const SHORT_ORDER: Duration = Duration::from_secs(1);

/// An order bound whose larger part preparation spends.
const SPENT_ORDER: Duration = Duration::from_secs(4);

/// The simulated preparation work spent from [`SPENT_ORDER`] before the
/// directory is reached.
const PREPARATION_WORK: Duration = Duration::from_secs(3);

/// How far past its captured expiry an order may end. Below
/// [`PREPARATION_WORK`], so an expiry taken again after preparation, at
/// `SPENT_ORDER + PREPARATION_WORK`, falls outside it; wide enough for a
/// loaded CI host's timer and scheduling lag.
const EXPIRY_TOLERANCE: Duration = Duration::from_secs(2);

/// How the directory peer answers a request that completed its handshake.
#[derive(Clone, Copy)]
enum Answer {
    /// A server error, so the client fails at once.
    Refuse,
    /// Nothing, until the peer finishes.
    Hang,
    /// The directory and fresh nonces, then this problem document for every
    /// other request, so the account request ends in it.
    Problem(ProblemAnswer),
}

/// One ACME problem document and the HTTP status it is sent under.
#[derive(Clone, Copy)]
struct ProblemAnswer {
    status: u16,
    body: &'static str,
}

/// How long a problem-answering peer waits for the rest of a request.
const REQUEST_READ_BOUND: Duration = Duration::from_secs(5);

/// The request lines a directory peer recorded.
type RequestLines = Box<[Box<str>]>;

/// A loopback TLS peer standing in for an ACME directory.
struct DirectoryPeer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Box<str>>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<std::io::Result<()>>>,
}

impl DirectoryPeer {
    fn start(pki: &Pki, answer: Answer) -> Result<Self, String> {
        let config = pki.server_config()?;
        let (listener, addr) = bind_loopback()?;
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            let port = addr.port();
            move || serve(&listener, &config, (answer, port), &requests, &stop)
        });
        Ok(Self {
            addr,
            requests,
            stop,
            thread: Some(thread),
        })
    }

    fn url(&self, host: &str) -> String {
        format!("https://{host}:{}/directory", self.addr.port())
    }

    /// Stop accepting and join the peer's thread, once.
    fn halt(&mut self) -> Option<std::thread::Result<std::io::Result<()>>> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().map(JoinHandle::join)
    }

    /// Take the request lines recorded so far, then stop accepting and join
    /// the peer's thread.
    fn finish(mut self) -> (RequestLines, Row) {
        let requests = std::mem::take(&mut *lock(&self.requests)).into_boxed_slice();
        let finished = match self.halt() {
            Some(Err(_)) => Err("the directory peer panicked".to_owned()),
            Some(Ok(Err(error))) => Err(format!("the directory peer's listener failed: {error}")),
            Some(Ok(Ok(()))) | None => Ok(()),
        };
        (requests, finished)
    }
}

impl Drop for DirectoryPeer {
    fn drop(&mut self) {
        drop(self.halt());
    }
}

/// Accept until stopped; each connection is served on its own thread so a
/// hanging answer never blocks the next handshake. `answer` carries the
/// peer's own port, which a served directory names. A connection thread's
/// panic is raised again once every thread is joined, so the peer's
/// [`DirectoryPeer::finish`] reports it, as it reports a failed listener.
fn serve(
    listener: &TcpListener,
    config: &Arc<rustls::ServerConfig>,
    answer: (Answer, u16),
    requests: &Arc<Mutex<Vec<Box<str>>>>,
    stop: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    let mut connections = Vec::new();
    let accepted = accept_until(
        listener,
        Duration::from_millis(5),
        || stop.load(Ordering::SeqCst),
        |stream| {
            let config = Arc::clone(config);
            let requests = Arc::clone(requests);
            let stop = Arc::clone(stop);
            connections.push(std::thread::spawn(move || {
                drop(connection(stream, config, answer, &requests, &stop));
            }));
        },
    );
    let panicked = connections
        .into_iter()
        .map(JoinHandle::join)
        .fold(None, |first, joined| first.or(joined.err()));
    if let Some(panic) = panicked {
        std::panic::resume_unwind(panic);
    }
    accepted
}

/// Complete one handshake, record the request line, and answer it.
fn connection(
    stream: std::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    (answer, port): (Answer, u16),
    requests: &Mutex<Vec<Box<str>>>,
    stop: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    let session = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(session, stream);
    let mut reader = BufReader::new(&mut tls);
    let mut line = String::new();
    loop {
        match reader.read_line(&mut line) {
            Ok(_) => break,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && !stop.load(Ordering::SeqCst) => {}
            Err(error) => return Err(error),
        }
    }
    let line: Box<str> = line.trim_end().into();
    lock(requests).push(line.clone());
    match answer {
        Answer::Refuse => tls.write_all(
            b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        ),
        Answer::Hang => {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        }
        Answer::Problem(problem) => {
            reader
                .get_ref()
                .sock
                .set_read_timeout(Some(REQUEST_READ_BOUND))?;
            drain_request(&mut reader)?;
            tls.write_all(&acme_answer(&line, port, problem))
        }
    }
}

/// Read the rest of one request: its headers, then the body they announce.
fn drain_request(reader: &mut impl BufRead) -> std::io::Result<()> {
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        match header.split_once(':') {
            Some((name, value)) if name.eq_ignore_ascii_case("content-length") => {
                length = value.trim().parse().map_err(std::io::Error::other)?;
            }
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)
}

/// The minimal ACME answer to `line` from the peer on `port`: the directory,
/// a fresh nonce, or `problem` for anything else.
fn acme_answer(line: &str, port: u16, problem: ProblemAnswer) -> Vec<u8> {
    let base = format!("https://localhost:{port}");
    let nonce = "replay-nonce: problem-peer-nonce\r\n";
    let mut parts = line.split_whitespace();
    let (status, headers, body) = match (parts.next(), parts.next()) {
        (Some("GET"), Some("/directory")) => (
            200,
            "content-type: application/json\r\n".to_owned(),
            format!(
                r#"{{"newNonce":"{base}/nonce","newAccount":"{base}/account","newOrder":"{base}/order"}}"#
            ),
        ),
        (Some("HEAD"), Some("/nonce")) => (200, nonce.to_owned(), String::new()),
        _ => (
            problem.status,
            format!("{nonce}content-type: application/problem+json\r\n"),
            problem.body.to_owned(),
        ),
    };
    format!(
        "HTTP/1.1 {status} Answer\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A provider whose preparation always succeeds and whose writes are inert.
struct InertProvider;

impl DnsProvider for InertProvider {
    fn prepare(
        &mut self,
        _domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        std::future::ready(Ok(()))
    }

    fn create_txt_record(
        &self,
        _fqdn: &str,
        _value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        std::future::ready(Ok(RecordId::from("inert")))
    }

    fn delete_txt_record(
        &self,
        _record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        std::future::ready(Ok(()))
    }
}

/// A provider whose preparation spends a fixed amount of work, then succeeds.
struct SlowPreparation;

impl DnsProvider for SlowPreparation {
    async fn prepare(&mut self, _: &[Arc<str>]) -> Result<(), RuntimeError> {
        tokio::time::sleep(PREPARATION_WORK).await;
        Ok(())
    }

    fn create_txt_record(
        &self,
        fqdn: &str,
        value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        InertProvider.create_txt_record(fqdn, value)
    }

    fn delete_txt_record(
        &self,
        record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        InertProvider.delete_txt_record(record_id)
    }
}

/// A DNS-01 configuration over a fresh cache in `root`.
fn acme(root: &TempDir) -> AcmeDns01 {
    AcmeDns01::new("camber", ["app.example.com"])
        .email("admin@example.com")
        .cache_dir(root.path().join("cache"))
}

/// The name a directory peer's leaf certifies.
const PEER_HOST: &str = "localhost";

/// A DNS-01 configuration over a fresh cache in `root`, ordering from `peer`
/// through `host` and trusting each of `roots`.
fn peer_configuration(
    root: &TempDir,
    peer: &DirectoryPeer,
    host: &str,
    roots: &[&str],
) -> Result<AcmeDns01, String> {
    acme(root)
        .directory_url(&peer.url(host))
        .and_then(|acme| {
            roots
                .iter()
                .try_fold(acme, |acme, pem| acme.add_root_certificate(pem.as_bytes()))
        })
        .map_err(|error| format!("configure: {error:?}"))
}

/// Provision under a fresh runtime, returning the outcome and its duration.
fn provision<P: DnsProvider + 'static>(
    acme: AcmeDns01,
    provider: P,
) -> Result<(Result<(), RuntimeError>, Duration), String> {
    let outcome = runtime::builder().run(move || {
        let started = Instant::now();
        bounded(
            "provision_cert",
            DNS_ROW_BOUND,
            acme.provision_cert(provider),
        )
        .map(|outcome| (outcome.map(drop), started.elapsed()))
    });
    clean_run(outcome)
}

const LOOKUP_TIMEOUT: Refusal = expired(IntegrationOperation::ZoneLookup);

const PROVISION_TIMEOUT: Refusal = expired(IntegrationOperation::Provision);

const INVALID_DIRECTORY: Refusal = invalid_config(IntegrationOperation::Provision);

/// A directory must be an absolute HTTPS URL, and a root must parse; each
/// refusal is configuration, found before anything runs.
fn directory_and_roots_are_validated() -> Row {
    let directory =
        |url: &str| refused(AcmeDns01::new("camber", ["app.example.com"]).directory_url(url));
    let root = |pem: &[u8]| {
        refused(AcmeDns01::new("camber", ["app.example.com"]).add_root_certificate(pem))
    };
    let pki = Pki::new("camber valid root")?;
    all([
        expect_eq(
            "a plain HTTP directory",
            directory("http://127.0.0.1:14000/dir"),
            Some(INVALID_DIRECTORY),
        ),
        expect_eq(
            "a directory with credentials",
            directory("https://user:pass@localhost/dir"),
            Some(INVALID_DIRECTORY),
        ),
        expect_eq(
            "a relative directory",
            directory("/directory"),
            Some(INVALID_DIRECTORY),
        ),
        expect_eq(
            "an HTTPS directory",
            directory("https://localhost/dir"),
            None,
        ),
        expect_eq(
            "a root that is not PEM",
            root(b"not a certificate"),
            Some(INVALID_DIRECTORY),
        ),
        expect_eq("an empty root", root(b""), Some(INVALID_DIRECTORY)),
        expect_eq("a PEM root", root(pki.root_pem().as_bytes()), None),
    ])
}

/// Whether the peer recorded the client's directory fetch.
fn reached_directory(requests: &[Box<str>]) -> bool {
    requests
        .iter()
        .any(|line| line.starts_with("GET /directory "))
}

/// Run one provisioning against a refusing directory peer reached through
/// `host`, trusting `roots`, and return what the peer recorded.
fn directory_requests(pki: &Pki, host: &str, roots: &[&str]) -> Result<RequestLines, String> {
    let peer = DirectoryPeer::start(pki, Answer::Refuse)?;
    let root = tempdir()?;
    let outcome = provision(
        peer_configuration(&root, &peer, host, roots)?,
        InertProvider,
    );
    let (requests, finished) = peer.finish();
    let (provisioned, _) = outcome?;
    finished?;
    match provisioned {
        Ok(()) => Err("provisioning succeeded against a refusing directory".to_owned()),
        Err(error) if is_configuration_refusal(&error) => {
            Err(format!("refused as config: {error:?}"))
        }
        Err(_) => Ok(requests),
    }
}

/// Configured roots are additive and reach the client that fetches the
/// directory; an untrusted chain or a host the leaf does not name is refused
/// in the handshake, before any request.
fn configured_trust_reaches_the_directory_client() -> Row {
    let pki = Pki::new("camber directory root")?;
    let other = Pki::new("camber unrelated root")?;
    let trusted = directory_requests(&pki, PEER_HOST, &[other.root_pem(), pki.root_pem()])?;
    let untrusted = directory_requests(&pki, PEER_HOST, &[other.root_pem()])?;
    let wrong_host = directory_requests(&pki, "127.0.0.1", &[pki.root_pem()])?;
    all([
        expect(
            "the trusted directory received its GET",
            reached_directory(&trusted),
        ),
        expect_eq(
            "requests through an untrusted chain",
            untrusted,
            RequestLines::default(),
        ),
        expect_eq(
            "requests to a host the leaf does not name",
            wrong_host,
            RequestLines::default(),
        ),
    ])
}

/// A Cloudflare redirect is refused, and the credential never reaches the
/// redirect target.
fn cloudflare_redirects_never_forward_credentials() -> Row {
    let driven = on_tokio(async {
        let target = MockServer::start().await;
        let origin = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/zones", target.uri())),
            )
            .mount(&origin)
            .await;
        let mut provider =
            CloudflareProvider::with_base_url("secret-token".into(), origin.uri().into())
                .map_err(|error| format!("descriptor: {error:?}"))?;
        let prepared = provider.prepare(&[Arc::from("app.example.com")]).await;
        let forwarded = target
            .received_requests()
            .await
            .ok_or("the redirect target records no requests")?
            .len();
        Ok::<_, String>((prepared, forwarded))
    });
    let (prepared, forwarded) = driven??;
    all([
        expect_eq(
            "the redirected lookup",
            refused(prepared),
            Some(rejected(IntegrationOperation::ZoneLookup)),
        ),
        expect_eq("requests reaching the redirect target", forwarded, 0),
    ])
}

/// An order bound below the request cap ends a hanging provider request at
/// the order's own expiry.
fn order_deadline_bounds_provider_requests() -> Row {
    let server = on_tokio(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(60)))
            .mount(&server)
            .await;
        server
    })?;
    let provider = CloudflareProvider::with_base_url("token".into(), server.uri().into())
        .map_err(|error| format!("descriptor: {error:?}"))?;
    let root = tempdir()?;
    let (outcome, elapsed) = provision(acme(&root).operation_timeout(SHORT_ORDER), provider)?;
    all([
        expect_eq("the hanging lookup", refused(outcome), Some(LOOKUP_TIMEOUT)),
        expect(
            &format!("the lookup ended at the order bound, not the cap: {elapsed:?}"),
            elapsed >= SHORT_ORDER && elapsed < REQUEST_CAP,
        ),
    ])
}

/// Provision under an order bound of `order` against a directory peer that
/// leaves its first request hanging. Returns the checks both hanging rows
/// share, with the refusal named `what`, and how long the order took.
fn hanging_directory<P: DnsProvider + 'static>(
    root_name: &str,
    order: Duration,
    provider: P,
    what: &str,
) -> Result<(Row, Duration), String> {
    let pki = Pki::new(root_name)?;
    let peer = DirectoryPeer::start(&pki, Answer::Hang)?;
    let root = tempdir()?;
    let configured =
        peer_configuration(&root, &peer, PEER_HOST, &[pki.root_pem()])?.operation_timeout(order);
    let outcome = provision(configured, provider);
    let (requests, finished) = peer.finish();
    let (outcome, elapsed) = outcome?;
    let shared = all([
        finished,
        expect(
            "the directory received the request it left hanging",
            reached_directory(&requests),
        ),
        expect_eq(what, refused(outcome), Some(PROVISION_TIMEOUT)),
    ]);
    Ok((shared, elapsed))
}

/// An order bound below the request cap ends a hanging directory request at
/// the order's own expiry. The hanging request belongs to the account step,
/// which the ACME transport alone bounds, so a transport that ignored the
/// order's expiry would wait out the request cap.
fn order_deadline_bounds_directory_requests() -> Row {
    let (shared, elapsed) = hanging_directory(
        "camber hanging root",
        SHORT_ORDER,
        InertProvider,
        "the hanging directory",
    )?;
    all([
        shared,
        expect(
            &format!("the request ended at the order bound, not the cap: {elapsed:?}"),
            elapsed >= SHORT_ORDER && elapsed < REQUEST_CAP,
        ),
    ])
}

/// The ACME request cap still applies when the order has more time left.
fn directory_requests_are_capped() -> Row {
    let (shared, elapsed) = hanging_directory(
        "camber capped directory root",
        REQUEST_CAP * 3,
        InertProvider,
        "the capped directory",
    )?;
    all([
        shared,
        expect(
            &format!("the directory request ended at its cap: {elapsed:?}"),
            elapsed >= REQUEST_CAP && elapsed < REQUEST_CAP * 2,
        ),
    ])
}

/// The order's expiry is fixed before preparation: when preparation spends
/// most of the bound, a hanging directory request ends at that expiry, not a
/// full bound after preparation finished. The ACME transport receives the
/// captured expiry; one taken again when the transport is built would let the
/// request run past it.
fn order_expiry_is_captured_before_preparation() -> Row {
    let (shared, elapsed) = hanging_directory(
        "camber spent root",
        SPENT_ORDER,
        SlowPreparation,
        "the hanging directory after preparation",
    )?;
    all([
        shared,
        expect(
            &format!("the order ended at its captured expiry: {elapsed:?}"),
            elapsed >= SPENT_ORDER && elapsed < SPENT_ORDER + EXPIRY_TOLERANCE,
        ),
    ])
}

const PROVISION_UNAVAILABLE: Refusal = unavailable(IntegrationOperation::Provision);

/// Provision against a directory that ends the account request in `problem`.
/// Returns the refusal, or a failure naming `what` when the account request
/// never reached the peer.
fn problem_refusal(what: &str, problem: ProblemAnswer) -> Result<Option<Refusal>, String> {
    let pki = Pki::new("camber problem root")?;
    let peer = DirectoryPeer::start(&pki, Answer::Problem(problem))?;
    let root = tempdir()?;
    let configured = peer_configuration(&root, &peer, PEER_HOST, &[pki.root_pem()])?;
    let outcome = provision(configured, InertProvider);
    let (requests, finished) = peer.finish();
    let (provisioned, _) = outcome?;
    finished?;
    expect(
        &format!("{what}: the account request reached the directory: {requests:?}"),
        requests
            .iter()
            .any(|line| line.starts_with("POST /account ")),
    )?;
    Ok(refused(provisioned))
}

/// A server failure cannot prove that the submitted account write had no effect.
#[test]
fn directory_problems_are_typed() -> Row {
    let rows = [
        (
            "a 503 problem",
            ProblemAnswer {
                status: 503,
                body: r#"{"detail":"maintenance","status":503}"#,
            },
            unknown(IntegrationOperation::Provision),
        ),
        (
            "a rateLimited problem",
            ProblemAnswer {
                status: 429,
                body: r#"{"type":"urn:ietf:params:acme:error:rateLimited","detail":"slow down"}"#,
            },
            PROVISION_UNAVAILABLE,
        ),
        (
            "a serverInternal problem",
            ProblemAnswer {
                status: 400,
                body: r#"{"type":"urn:ietf:params:acme:error:serverInternal"}"#,
            },
            unknown(IntegrationOperation::Provision),
        ),
        (
            "a badNonce problem",
            ProblemAnswer {
                status: 400,
                body: r#"{"type":"urn:ietf:params:acme:error:badNonce","status":400}"#,
            },
            PROVISION_UNAVAILABLE,
        ),
        (
            "a malformed problem",
            ProblemAnswer {
                status: 400,
                body: r#"{"type":"urn:ietf:params:acme:error:malformed","status":400}"#,
            },
            rejected(IntegrationOperation::Provision),
        ),
    ];
    all(rows.into_iter().map(|(what, problem, expected)| {
        expect_eq(what, problem_refusal(what, problem)?, Some(expected))
    }))
}

#[test]
fn submitted_acme_writes_do_not_claim_safe_retry() {
    run_rows(&[
        ("lost challenge answer", || {
            uncertain_acme_write(Scripts {
                challenge: Script::every(Stage::Lose),
                ..Scripts::default()
            })
        }),
        ("lost finalize answer", || {
            uncertain_acme_write(Scripts {
                finalize: Stage::Lose,
                ..Scripts::default()
            })
        }),
    ]);
}

fn uncertain_acme_write(scripts: Scripts) -> Row {
    let fixture = Fixture::start(scripts)?;
    let result = provision(
        fixture.configuration(&TWO_ZONES)?,
        fixture.peers.provider()?,
    )?;
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        expect_eq(
            "submitted write",
            refused(result.0),
            Some(unknown(IntegrationOperation::Provision)),
        ),
        expect(
            "no challenge reached the directory",
            fixture.peers.acme.log().challenges > 0,
        ),
        expect_exact_deletes(&log, &[]),
        expect(
            "failed issuance published a certificate",
            fixture.bundle().is_none(),
        ),
    ]);
    all([checks, fixture.finish()])
}

#[test]
fn lost_post_as_get_response_remains_safe_to_retry() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    fixture.peers.acme.authorization_answer(Stage::Lose);
    let result = provision(
        fixture.configuration(&TWO_ZONES)?,
        fixture.peers.provider()?,
    )?;
    let checks = all([
        expect_eq("lost read", refused(result.0), Some(PROVISION_UNAVAILABLE)),
        expect(
            "the read never reached the peer",
            fixture.peers.acme.log().authorizations > 0,
        ),
        expect_eq("challenge writes", fixture.peers.acme.log().challenges, 0),
        expect_eq(
            "TXT creates",
            fixture.peers.cloudflare.log().creates.len(),
            0,
        ),
    ]);
    all([checks, fixture.finish()])
}

/// Copy the account file `from` one cache `to` another.
fn carry_account(from: &Path, to: &Path) -> Row {
    std::fs::create_dir_all(to)
        .and_then(|()| std::fs::copy(from.join(ACCOUNT_FILE), to.join(ACCOUNT_FILE)))
        .map(drop)
        .map_err(|error| format!("carry the account file: {error}"))
}

/// The directory the account file in `cache` names.
fn stored_directory(cache: &Path) -> Result<Option<String>, String> {
    let stored = std::fs::read(cache.join(ACCOUNT_FILE))
        .map_err(|error| format!("read the account file: {error}"))?;
    let credentials: serde_json::Value = serde_json::from_slice(&stored)
        .map_err(|error| format!("parse the account file: {error}"))?;
    Ok(credentials["directory"].as_str().map(str::to_owned))
}

/// An account stored for another directory does not follow a configuration
/// that names a new one: the configured directory registers the account and
/// issues the order, and the account file names it afterwards.
fn stored_account_follows_the_configured_directory() -> Row {
    let previous = Fixture::start(Scripts::default())?;
    let current = Fixture::start(Scripts::default())?;
    expect("a fresh cache already exists", !previous.cache().exists())?;
    let (registered, _) = provision(
        previous.configuration(&TWO_ZONES)?,
        previous.peers.provider()?,
    )?;
    carry_account(previous.cache(), current.cache())?;
    #[cfg(unix)]
    let original_inode = seed_permissive_account(current.cache())?;
    let (reconfigured, _) = provision(
        current.configuration(&TWO_ZONES)?,
        current.peers.provider()?,
    )?;
    let checks = all([
        #[cfg(unix)]
        check_account_replacement(current.cache(), original_inode),
        expect(
            &format!("the registration on the previous directory: {registered:?}"),
            registered.is_ok(),
        ),
        expect(
            &format!("the order under the configured directory: {reconfigured:?}"),
            reconfigured.is_ok(),
        ),
        expect(
            "the configured directory issued the order",
            current.peers.acme.log().issued.is_some(),
        ),
        expect_eq(
            "the directory the cached account names",
            stored_directory(current.cache())?,
            Some(current.peers.acme.directory_url()),
        ),
    ]);
    all([checks, previous.finish(), current.finish()])
}

#[cfg(unix)]
fn seed_permissive_account(cache: &Path) -> Result<u64, String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let account = cache.join(ACCOUNT_FILE);
    std::fs::set_permissions(&account, std::fs::Permissions::from_mode(0o644))
        .map_err(|error| error.to_string())?;
    std::fs::metadata(account)
        .map(|metadata| metadata.ino())
        .map_err(|error| error.to_string())
}

#[cfg(unix)]
fn check_account_replacement(cache: &Path, original_inode: u64) -> Row {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata =
        std::fs::metadata(cache.join(ACCOUNT_FILE)).map_err(|error| error.to_string())?;
    all([
        expect(
            "account replacement did not rename a new file",
            metadata.ino() != original_inode,
        ),
        expect_eq(
            "account permissions",
            metadata.permissions().mode() & 0o777,
            0o600,
        ),
    ])
}

/// With no order around it, one provider request is capped at 10 seconds.
fn provider_requests_are_capped() -> Row {
    let (listener, addr) = bind_loopback()?;
    let driven = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .map_err(|error| format!("paused runtime: {error}"))?
        .block_on(async {
            let mut provider =
                CloudflareProvider::with_base_url("token".into(), format!("http://{addr}").into())
                    .map_err(|error| format!("descriptor: {error:?}"))?;
            let started = tokio::time::Instant::now();
            let prepared = provider.prepare(&[Arc::from("app.example.com")]).await;
            Ok::<_, String>((prepared, started.elapsed()))
        });
    drop(listener);
    let (prepared, elapsed) = driven?;
    all([
        expect_eq("the silent lookup", refused(prepared), Some(LOOKUP_TIMEOUT)),
        expect(
            &format!("the request ended at the cap: {elapsed:?}"),
            elapsed >= REQUEST_CAP && elapsed < REQUEST_CAP + Duration::from_secs(1),
        ),
    ])
}

#[test]
fn dns_directory_trust_and_request_deadlines_reach_the_real_client() {
    run_rows(&[
        (
            "the directory and roots are validated",
            directory_and_roots_are_validated,
        ),
        (
            "configured trust reaches the directory client",
            configured_trust_reaches_the_directory_client,
        ),
        (
            "Cloudflare redirects never forward credentials",
            cloudflare_redirects_never_forward_credentials,
        ),
        (
            "the order deadline bounds provider requests",
            order_deadline_bounds_provider_requests,
        ),
        (
            "the order deadline bounds directory requests",
            order_deadline_bounds_directory_requests,
        ),
        (
            "the order expiry is captured before preparation",
            order_expiry_is_captured_before_preparation,
        ),
        ("provider requests are capped", provider_requests_are_capped),
        (
            "directory requests are capped",
            directory_requests_are_capped,
        ),
        ("directory problems are typed", directory_problems_are_typed),
        (
            "a stored account follows the configured directory",
            stored_account_follows_the_configured_directory,
        ),
    ]);
}
