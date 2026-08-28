use crate::common::{
    BOUND, BoundedReadError, PERPETUAL, SHORT_DRAIN, block_on_detached, join_thread_bounded,
    prove_scope_owned,
};
use crate::scope_builders::scope_runtime;
use camber::{RuntimeError, http, runtime};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// How often the upstream's accept loop rechecks its stop flag.
///
/// Teardown is bounded by this plus one probe's own read and write bounds: the
/// loop never blocks in `accept`, so the join it ends at cannot outlive them.
const ACCEPT_POLL: Duration = Duration::from_millis(5);

/// The path this upstream refuses.
///
/// `spawn_health_checker` seeds its flag `true` before probing, so a run that
/// only ever probes a healthy path cannot tell a published verdict from that
/// seed. This is the path that gives the flag a false to carry.
///
/// Coupled with [`UNHEALTHY_REQUEST_LINE`]: change one and change the other.
const UNHEALTHY_PATH: &str = "/down";

/// The start of the request line [`UNHEALTHY_PATH`] arrives on.
///
/// Spelled out as bytes so the upstream matches a constant rather than
/// formatting the same request line again on every probe it serves. It sits
/// against the path it repeats so the pair cannot drift unnoticed.
const UNHEALTHY_REQUEST_LINE: &[u8] = b"GET /down ";

/// What the upstream answers a healthy path with.
const HEALTHY_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// What the upstream answers [`UNHEALTHY_PATH`] with.
const UNHEALTHY_RESPONSE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// An HTTP/1.1 upstream on a background thread, answering by request target.
///
/// Every response closes its connection, so one non-blocking accept loop serves
/// every probe in order and the probe count is exact.
struct StaticUpstream {
    addr: SocketAddr,
    probes: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StaticUpstream {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let probes = Arc::new(AtomicUsize::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_probes = Arc::clone(&probes);
        let thread_stopped = Arc::clone(&stopped);

        let thread = std::thread::spawn(move || {
            while !thread_stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        thread_probes.fetch_add(1, Ordering::SeqCst);
                        serve_one_probe(stream);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ACCEPT_POLL)
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            addr,
            probes,
            stopped,
            thread: Some(thread),
        }
    }

    fn url(&self) -> Box<str> {
        format!("http://{}", self.addr).into_boxed_str()
    }

    fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }

    /// Stop the accept loop and join its thread, bounded.
    ///
    /// Every case ends here rather than leaving teardown to `Drop`, which
    /// cannot report anything: a thread still inside a probe read — whose
    /// byte-at-a-time loop is bounded per read, not overall — becomes a named
    /// failure instead of a join that never returns.
    fn stop(mut self) -> Result<(), BoundedReadError> {
        self.stopped.store(true, Ordering::SeqCst);
        join_thread_bounded(&mut self.thread, BOUND)
    }
}

impl Drop for StaticUpstream {
    /// The safety net for a case that failed before reaching
    /// [`StaticUpstream::stop`]. Bounded like that path, because `Drop` has no
    /// way to report a thread that never came back.
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        // The loop rechecks this flag between non-blocking accepts, so it ends
        // itself within one poll interval unless a probe read is still holding
        // it.
        let _ = join_thread_bounded(&mut self.thread, BOUND);
    }
}

fn serve_one_probe(mut stream: TcpStream) {
    // The bounds are asserted, not attempted: a discarded failure would leave
    // the byte-at-a-time loop below with no deadline at all, and the teardown
    // join waiting on it with none either.
    stream
        .set_nonblocking(false)
        .expect("probe socket rejected blocking mode");
    stream
        .set_read_timeout(Some(BOUND))
        .expect("probe socket rejected its read bound");
    stream
        .set_write_timeout(Some(BOUND))
        .expect("probe socket rejected its write bound");
    let mut request = Vec::new();
    let mut byte = [0u8; 1];
    while !request.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return,
            Ok(_) => request.push(byte[0]),
        }
    }
    let _ = stream.write_all(probe_response(&request));
    let _ = stream.flush();
}

/// The answer this upstream owes the request it just read.
fn probe_response(request: &[u8]) -> &'static [u8] {
    match request.starts_with(UNHEALTHY_REQUEST_LINE) {
        true => UNHEALTHY_RESPONSE,
        false => HEALTHY_RESPONSE,
    }
}

/// `spawn_health_checker`'s loop is a root-scope child: admitted while it
/// runs, and removed when `ScopeClosing` ends it. A detached loop that merely
/// observed closing would never reach the registry at all.
#[test]
fn spawn_health_checker_exits_on_scope_closing() {
    let upstream = StaticUpstream::start();
    let url = upstream.url();

    // The shared fixture holds the drain at a count of one, so the registry
    // reading it takes proves every closing-observing child was removed.
    let (proof, healthy) = prove_scope_owned(
        BOUND,
        |builder| builder,
        move |subjects| {
            subjects.name_subsystem("health checker");
            let checker = http::spawn_health_checker(&url, "/", PERPETUAL);
            runtime::block_on(checker).unwrap().load(Ordering::Acquire)
        },
    );

    proof.assert_owned("spawn_health_checker's loop");
    assert!(healthy, "the deterministic upstream did not report healthy");
    // Exactly one: the initial probe runs before admission and `PERPETUAL`
    // is long enough that the admitted loop cannot add a second, so a loop that
    // probed again on entry would change this count.
    assert_eq!(
        upstream.probes(),
        1,
        "the initial probe did not run exactly once before admission"
    );
    upstream
        .stop()
        .expect("the upstream accept loop never ended");
}

/// The flag the checker hands back carries its initial probe's verdict, not the
/// optimistic value it was seeded with. The paired healthy case cannot show
/// this on its own: there, a published `true` and an unwritten seed are the
/// same value.
#[test]
fn spawn_health_checker_publishes_a_refused_initial_probe() {
    let upstream = StaticUpstream::start();
    let url = upstream.url();

    let (proof, healthy) = prove_scope_owned(
        BOUND,
        |builder| builder,
        move |subjects| {
            subjects.name_subsystem("health checker");
            let checker = http::spawn_health_checker(&url, UNHEALTHY_PATH, PERPETUAL);
            runtime::block_on(checker).unwrap().load(Ordering::Acquire)
        },
    );

    // Admitted on the same terms as a healthy backend: a refusing upstream is
    // a state the loop reports, not a reason to skip the loop.
    proof.assert_owned("spawn_health_checker's loop");
    assert!(!healthy, "the refusing upstream was reported healthy");
    assert_eq!(
        upstream.probes(),
        1,
        "the initial probe did not run exactly once before admission"
    );
    upstream
        .stop()
        .expect("the upstream accept loop never ended");
}

/// With no runtime context the checker is refused before it probes: absence
/// is a typed outcome, not a runtime the call mints for itself.
#[test]
fn spawn_health_checker_without_a_runtime_is_refused() {
    let upstream = StaticUpstream::start();

    // A Tokio runtime with no Camber context: the checker gets the reactor its
    // probe needs and nothing else, so the absence under test is genuine.
    let result = block_on_detached(http::spawn_health_checker(&upstream.url(), "/", PERPETUAL));

    assert!(
        matches!(result, Err(RuntimeError::NoRuntime)),
        "expected NoRuntime with no runtime context"
    );
    assert_eq!(
        upstream.probes(),
        0,
        "the refused checker probed the upstream anyway"
    );
    upstream
        .stop()
        .expect("the upstream accept loop never ended");
}

/// Once the root scope has closed, the checker is refused: a perpetual loop
/// with no owner left to await it is never started.
#[test]
fn spawn_health_checker_after_scope_close_is_refused() {
    let upstream = StaticUpstream::start();
    let url = upstream.url();

    let result = scope_runtime(SHORT_DRAIN)
        .run(move || {
            runtime::request_shutdown();
            runtime::block_on(http::spawn_health_checker(&url, "/", PERPETUAL))
        })
        .unwrap();

    assert!(
        matches!(result, Err(RuntimeError::ScopeClosed)),
        "expected ScopeClosed after the root scope closed to admission"
    );
    // The refusal happened at admission, not before it: runtime context
    // resolved and the initial probe ran, which is what separates this case
    // from the no-runtime one.
    assert_eq!(
        upstream.probes(),
        1,
        "the closed-scope refusal did not run the initial probe first"
    );
    upstream
        .stop()
        .expect("the upstream accept loop never ended");
}
