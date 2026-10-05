//! Local service fixture lifecycle against a controlled container engine.
//!
//! The engine is a scripted `docker` whose state lives in a directory this
//! test owns, so every row runs without a real engine and never skips. The
//! published endpoints are loopback peers in this process that speak just
//! enough protocol to acknowledge or refuse readiness, and that record whether
//! the fixture closed every connection it opened. Real service readiness
//! belongs to the lanes that start real services.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::http::poll_until;
use crate::local_integrations::FixtureError;
use crate::local_integrations::engine::Engine;
use crate::local_integrations::engine_stub::{EngineStub, MISSING_IMAGE_PREFIX, process_alive};
use crate::local_integrations::readiness::Readiness;
use crate::local_integrations::service::{
    LocalServices, ServiceFile, ServiceSpec, StartFailure, Teardown,
};

const READINESS_BOUND: Duration = Duration::from_secs(10);
/// Short on purpose: the refusing peer never acknowledges, so this row always
/// spends its whole bound.
const REFUSED_READINESS_BOUND: Duration = Duration::from_millis(300);
const PEER_BOUND: Duration = Duration::from_secs(10);
const FOREIGN_CONTAINER: &str = "foreign-service";
const FOREIGN_NETWORK: &str = "foreign-network";
const NATS_IMAGE: &str =
    "fixture.invalid/nats@sha256:1111111111111111111111111111111111111111111111111111111111111111";
const QUEUE_IMAGE: &str = "fixture.invalid/elasticmq@sha256:2222222222222222222222222222222222222222222222222222222222222222";
const QUEUE_CONFIG: &[u8] = b"include classpath(\"application.conf\")\nqueues {}\n";

// --- Loopback protocol peers ---------------------------------------------

#[derive(Clone, Copy, Debug)]
enum PeerScript {
    NatsAcknowledges,
    NatsRefuses,
    QueueAnswers,
}

/// What a peer saw: connections it accepted, and how many of those the
/// fixture closed before the peer's read bound expired.
#[derive(Debug, Default)]
struct PeerAccount {
    accepted: usize,
    closed: usize,
}

struct Peer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<PeerAccount>,
}

impl Peer {
    fn start(script: PeerScript) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("peer listener was not bound");
        let address = listener.local_addr().expect("peer address is readable");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut account = PeerAccount::default();
            loop {
                let (stream, _) = listener.accept().expect("peer accept failed");
                if stopping.load(Ordering::Acquire) {
                    return account;
                }
                account.accepted += 1;
                if serve(script, stream) {
                    account.closed += 1;
                }
            }
        });
        Self {
            address,
            stop,
            thread,
        }
    }

    /// Stop accepting and return what the peer saw.
    fn finish(self) -> PeerAccount {
        self.stop.store(true, Ordering::Release);
        // A wake that never connects would leave the join waiting forever.
        drop(TcpStream::connect(self.address).expect("the stop wakes the peer's accept"));
        self.thread.join().expect("peer thread panicked")
    }
}

/// Serve one connection, then report whether the fixture closed it.
///
/// A probe abandoned at its readiness deadline closes before the exchange
/// ends, so the close is observed whether or not the exchange completed.
fn serve(script: PeerScript, stream: TcpStream) -> bool {
    stream
        .set_read_timeout(Some(PEER_BOUND))
        .expect("peer read bound was not set");
    let mut writer = stream.try_clone().expect("peer stream was not cloned");
    let mut reader = BufReader::new(stream);
    match script {
        PeerScript::NatsAcknowledges => nats_exchange(&mut reader, &mut writer, b"PONG\r\n"),
        PeerScript::NatsRefuses => nats_exchange(
            &mut reader,
            &mut writer,
            b"-ERR 'Authorization Violation'\r\n",
        ),
        PeerScript::QueueAnswers => queue_exchange(&mut reader, &mut writer),
    };
    observed_close(&mut reader)
}

fn nats_exchange(reader: &mut impl BufRead, writer: &mut TcpStream, reply: &[u8]) -> bool {
    let mut line = String::new();
    writer
        .write_all(b"INFO {\"server_id\":\"fixture\",\"max_payload\":1048576}\r\n")
        .is_ok()
        && reader.read_line(&mut line).is_ok_and(|count| count > 0)
        && line.starts_with("CONNECT ")
        && {
            line.clear();
            reader.read_line(&mut line).is_ok_and(|count| count > 0)
        }
        && line.trim_end() == "PING"
        && writer.write_all(reply).is_ok()
}

/// Read a request head and return its body length: zero when no
/// `Content-Length` is sent, `None` when the head is cut short or the length
/// is malformed.
fn content_length(reader: &mut impl BufRead) -> Option<usize> {
    let mut line = String::new();
    let mut length = Some(0_usize);
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        let header = line.trim_end();
        if header.is_empty() {
            return length;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
}

fn queue_exchange(reader: &mut impl BufRead, writer: &mut TcpStream) -> bool {
    let Some(length) = content_length(reader) else {
        return false;
    };
    let mut body = vec![0_u8; length];
    let body_read = reader.read_exact(&mut body).is_ok();
    let reply = b"<ListQueuesResponse><ListQueuesResult/></ListQueuesResponse>";
    body_read
        && body.starts_with(b"Action=ListQueues")
        && writer
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    reply.len()
                )
                .as_bytes(),
            )
            .is_ok()
        && writer.write_all(reply).is_ok()
        && writer.shutdown(Shutdown::Write).is_ok()
}

/// Whether the fixture closed its end inside the peer's read bound.
///
/// A reset is the fixture's close too: a socket closed before the peer's
/// write lands answers that write with a reset.
fn observed_close(reader: &mut impl Read) -> bool {
    let mut chunk = [0_u8; 256];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) => {
                return matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                );
            }
        }
    }
}

// --- Owned-resource checks -----------------------------------------------

/// What one row expected the fixture to own.
struct Owned {
    network: Box<str>,
    containers: Box<[Box<str>]>,
    root: Option<PathBuf>,
}

fn nats_spec() -> ServiceSpec {
    ServiceSpec::new("nats", NATS_IMAGE, 4222, Readiness::NatsPing)
}

fn queue_spec() -> ServiceSpec {
    ServiceSpec {
        files: Box::new([ServiceFile {
            name: "elasticmq.conf".into(),
            contents: QUEUE_CONFIG.into(),
            container_path: "/opt/elasticmq.conf".into(),
        }]),
        ..ServiceSpec::new("queue", QUEUE_IMAGE, 9324, Readiness::sqs_queue_query())
    }
}

fn expected(run_id: &str, services: &[&str]) -> Owned {
    Owned {
        network: format!("camber-local-{run_id}").into(),
        containers: services
            .iter()
            .map(|service| format!("camber-local-{run_id}-{service}").into())
            .collect(),
        root: None,
    }
}

/// Every owned network, container, backing process, and root is gone, and
/// the teardown witness names exactly what was owned.
fn check_reaped(
    row: &str,
    stub: &EngineStub,
    owned: &Owned,
    witness: Option<&Teardown>,
    failures: &mut Vec<String>,
) {
    if let Some(witness) = witness {
        check_witness(row, owned, witness, failures);
    }
    if stub.networks().contains(&owned.network) {
        failures.push(format!("{row}: owned network {} survived", owned.network));
    }
    let live = stub.containers();
    for container in &owned.containers {
        if live.contains(container) {
            failures.push(format!("{row}: owned container {container} survived"));
        }
    }
    for started in stub
        .started()
        .iter()
        .filter(|started| owned.containers.contains(&started.name))
    {
        let gone = poll_until(PEER_BOUND, || !process_alive(started.process));
        if !gone {
            failures.push(format!(
                "{row}: backing process {} of {} survived",
                started.process, started.name
            ));
        }
    }
    let root = owned
        .root
        .as_ref()
        .or_else(|| witness.and_then(|witness| witness.root.as_ref()));
    match root {
        Some(root) if root.exists() => {
            failures.push(format!("{row}: root {} survived", root.display()));
        }
        Some(_) => {}
        None => failures.push(format!("{row}: no root was observed to check")),
    }
}

fn check_witness(row: &str, owned: &Owned, witness: &Teardown, failures: &mut Vec<String>) {
    if witness.network.as_deref() != Some(&*owned.network) {
        failures.push(format!(
            "{row}: teardown witness named network {:?}, expected {:?}",
            witness.network, owned.network
        ));
    }
    if witness.containers != owned.containers {
        failures.push(format!(
            "{row}: teardown witness named containers {:?}, expected {:?}",
            witness.containers, owned.containers
        ));
    }
    if witness.root.is_none() {
        failures.push(format!("{row}: teardown witness named no root"));
    }
}

fn check_cleanup(
    row: &str,
    stub: &EngineStub,
    owned: &Owned,
    cleanup: Result<Teardown, FixtureError>,
    failures: &mut Vec<String>,
) {
    match cleanup {
        Ok(witness) => check_reaped(row, stub, owned, Some(&witness), failures),
        Err(error) => failures.push(format!("{row}: teardown failed: {error}")),
    }
}

fn check_started(
    row: &str,
    services: &LocalServices,
    stub: &EngineStub,
    owned: &mut Owned,
    endpoints: (SocketAddr, SocketAddr),
    failures: &mut Vec<String>,
) {
    let (nats, queue) = endpoints;
    if services.endpoint("nats") != Some(nats) || services.endpoint("queue") != Some(queue) {
        failures.push(format!(
            "{row}: endpoints {:?}/{:?} are not the published peers {nats}/{queue}",
            services.endpoint("nats"),
            services.endpoint("queue"),
        ));
    }
    if services.network() != Some(&*owned.network) || services.container_names() != owned.containers
    {
        failures.push(format!(
            "{row}: fixture owns {:?} and {:?}, expected {:?} and {:?}",
            services.network(),
            services.container_names(),
            owned.network,
            owned.containers
        ));
    }
    let live = stub.containers();
    if !owned.containers.iter().all(|name| live.contains(name))
        || !stub.networks().contains(&owned.network)
    {
        failures.push(format!("{row}: engine does not hold the started resources"));
    }
    owned.root = services.root().map(PathBuf::from);
    let configured = owned
        .root
        .as_ref()
        .is_some_and(|root| root.join("elasticmq.conf").is_file());
    if !configured {
        failures.push(format!("{row}: service file was not written into the root"));
    }
}

fn check_peer(row: &str, account: &PeerAccount, failures: &mut Vec<String>) {
    if account.accepted == 0 {
        failures.push(format!("{row}: readiness never reached its peer"));
    }
    if account.closed != account.accepted {
        failures.push(format!(
            "{row}: fixture left {} of {} probe connections open",
            account.accepted - account.closed,
            account.accepted
        ));
    }
}

// --- Rows ----------------------------------------------------------------

/// Start two services, observe them owned while running, and finish.
fn start_then_finish(stub: &EngineStub, failures: &mut Vec<String>) {
    const ROW: &str = "explicit finish";
    let run_id = "finish";
    let nats = Peer::start(PeerScript::NatsAcknowledges);
    let queue = Peer::start(PeerScript::QueueAnswers);
    stub.publish_endpoint("nats", nats.address);
    stub.publish_endpoint("queue", queue.address);
    let mut owned = expected(run_id, &["nats", "queue"]);

    match LocalServices::start(
        Engine::at(stub.program()),
        run_id,
        &[nats_spec(), queue_spec()],
        READINESS_BOUND,
    ) {
        Ok(services) => {
            check_started(
                ROW,
                &services,
                stub,
                &mut owned,
                (nats.address, queue.address),
                failures,
            );
            check_cleanup(ROW, stub, &owned, services.finish(), failures);
        }
        Err(failure) => failures.push(format!("{ROW}: start failed: {}", failure.error)),
    }
    check_peer(&format!("{ROW} nats"), &nats.finish(), failures);
    check_peer(&format!("{ROW} queue"), &queue.finish(), failures);
}

/// The start refusal a row expects.
struct ExpectedRefusal {
    /// What an unexpected start means.
    started: &'static str,
    /// The expected failure, as a mismatch names it.
    named: &'static str,
    /// Whether a start failure is the expected one.
    matches: fn(&FixtureError) -> bool,
}

/// A start the row expects refused: the failure is the expected one, and its
/// cleanup reaped everything owned.
fn check_refused_start(
    row: &str,
    stub: &EngineStub,
    owned: &Owned,
    start: Result<LocalServices, StartFailure>,
    expected: &ExpectedRefusal,
    failures: &mut Vec<String>,
) {
    match start {
        Ok(services) => {
            failures.push(format!("{row}: {}", expected.started));
            drop(services.finish());
        }
        Err(StartFailure { error, cleanup }) => {
            failures.extend(
                (!(expected.matches)(&error))
                    .then(|| format!("{row}: failure was not the {}: {error}", expected.named)),
            );
            check_cleanup(row, stub, owned, cleanup, failures);
        }
    }
}

/// A refused readiness acknowledgement tears down before start returns.
fn readiness_failure(stub: &EngineStub, failures: &mut Vec<String>) {
    const ROW: &str = "readiness failure";
    let run_id = "refused";
    let nats = Peer::start(PeerScript::NatsRefuses);
    stub.publish_endpoint("nats", nats.address);
    let owned = expected(run_id, &["nats"]);

    let start = LocalServices::start(
        Engine::at(stub.program()),
        run_id,
        &[nats_spec()],
        REFUSED_READINESS_BOUND,
    );
    let refusal = ExpectedRefusal {
        started: "a refused PING read as ready",
        named: "nats readiness",
        matches: |error| matches!(error, FixtureError::Readiness { service, .. } if &**service == "nats"),
    };
    check_refused_start(ROW, stub, &owned, start, &refusal, failures);
    check_peer(ROW, &nats.finish(), failures);
}

/// An engine refusal part-way through start reaps what already started.
fn start_failure(stub: &EngineStub, failures: &mut Vec<String>) {
    const ROW: &str = "start failure";
    let run_id = "refused-image";
    let nats = Peer::start(PeerScript::NatsAcknowledges);
    stub.publish_endpoint("nats", nats.address);
    let owned = expected(run_id, &["nats", "missing"]);
    let missing = ServiceSpec::new(
        "missing",
        &format!("{MISSING_IMAGE_PREFIX}service@sha256:3333"),
        80,
        Readiness::NatsPing,
    );

    let start = LocalServices::start(
        Engine::at(stub.program()),
        run_id,
        &[nats_spec(), missing],
        READINESS_BOUND,
    );
    let refusal = ExpectedRefusal {
        started: "an absent image started",
        named: "engine refusal",
        matches: |error| matches!(error, FixtureError::Engine { .. }),
    };
    check_refused_start(ROW, stub, &owned, start, &refusal, failures);
    // Nothing reached readiness, so the peer may see no connection at all.
    nats.finish();
}

/// An assertion that unwinds out of the owning scope still reaps through
/// `Drop`.
fn assertion_unwind(stub: &EngineStub, failures: &mut Vec<String>) {
    const ROW: &str = "assertion unwind";
    let run_id = "unwound";
    let nats = Peer::start(PeerScript::NatsAcknowledges);
    stub.publish_endpoint("nats", nats.address);
    let mut owned = expected(run_id, &["nats"]);
    let mut observed_root: Option<PathBuf> = None;

    let unwound = panic::catch_unwind(AssertUnwindSafe(|| {
        let services = LocalServices::start(
            Engine::at(stub.program()),
            run_id,
            &[nats_spec()],
            READINESS_BOUND,
        )
        .unwrap_or_else(|failure| panic!("{ROW}: start failed: {}", failure.error));
        observed_root = services.root().map(PathBuf::from);
        panic!("{ROW}: a fixture assertion failed inside the owning scope");
    }));

    if unwound.is_ok() {
        failures.push(format!("{ROW}: the owning scope did not unwind"));
    }
    match observed_root {
        Some(root) => {
            owned.root = Some(root);
            check_reaped(ROW, stub, &owned, None, failures);
        }
        None => failures.push(format!("{ROW}: the fixture never started")),
    }
    check_peer(ROW, &nats.finish(), failures);
}

#[test]
fn local_fixture_failure_reaps_only_owned_resources() {
    let stub = EngineStub::new();
    let foreign_process = stub.seed_foreign(FOREIGN_CONTAINER, FOREIGN_NETWORK);
    let mut failures = Vec::new();

    start_then_finish(&stub, &mut failures);
    readiness_failure(&stub, &mut failures);
    start_failure(&stub, &mut failures);
    assertion_unwind(&stub, &mut failures);

    if !stub.containers().contains(&Box::from(FOREIGN_CONTAINER)) {
        failures.push("a fixture teardown removed the foreign container".to_owned());
    }
    if !stub.networks().contains(&Box::from(FOREIGN_NETWORK)) {
        failures.push("a fixture teardown removed the foreign network".to_owned());
    }
    if !process_alive(foreign_process) {
        failures.push("a fixture teardown stopped the foreign container's process".to_owned());
    }
    let calls = stub.calls();
    stub.close();

    assert!(
        failures.is_empty(),
        "local fixture lifecycle rows failed:\n{}\n\nengine calls:\n{calls}",
        failures.join("\n")
    );
}
