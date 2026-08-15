use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use camber::http::{HostRouter, Router};
use camber::{JoinHandle, RuntimeError, http, runtime, spawn};

use super::http::wait_for_http_response;

/// How long a spawned fixture server has to answer its first probe.
const READINESS_BOUND: Duration = Duration::from_secs(5);

/// How long the readiness diagnosis waits for the serve call's own exit report.
///
/// Short, because by the time it is taken the report has either been sent or is
/// never coming. The server has just failed a five-second readiness wait, and a
/// `camber::spawn` the scope refused never ran the closure at all, so nothing
/// will ever send on that channel. The wait is here for the one case in between:
/// a serve call that returned just after the readiness bound expired still has
/// its own failure read out instead of being described as running.
const EXIT_REPORT_BOUND: Duration = Duration::from_millis(100);

pub fn test_runtime() -> runtime::RuntimeBuilder {
    runtime::builder()
        .header_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(1))
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

/// Bind an ephemeral port, serve it the way `serve` says, and wait until it
/// answers.
///
/// The reservation, the address it reports, and the readiness wait are the
/// whole spawn; which server function is handed the listener is the only thing
/// a caller varies. Stated once because a second copy of the spawn is a second
/// place the readiness bound and the address the caller is given can drift.
fn spawn_bound(
    serve: impl FnOnce(camber::net::Listener) -> Result<(), RuntimeError> + Send + 'static,
) -> SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let local_addr = listener.local_addr().unwrap().tcp().unwrap();
    let (exited_tx, exited) = channel();
    let served = spawn(move || -> Result<(), RuntimeError> {
        let outcome = serve(listener);
        // Sent before the outcome is handed back, so a readiness failure can
        // tell "the serve call has returned" from "it is still running" without
        // waiting on either.
        let _ = exited_tx.send(());
        outcome
    });
    match wait_for_http_response(local_addr, READINESS_BOUND) {
        Ok(_) => local_addr,
        Err(unready) => panic!("{}", unready_cause(served, &exited, &unready)),
    }
}

/// Why a spawned server never answered, with the serve call's own failure in it
/// when that call has one.
///
/// A `serve_listener` or `serve_hosts` that refuses reaches the caller as a
/// readiness timeout on the next line and nothing else, which names the symptom
/// and loses the cause. This is [`super::http`]'s `cancel_unready` rule for the
/// runtime-owned spawn: the two faults are reported as one, and neither
/// displaces the other. The join is taken only once the exit report has arrived,
/// so a serve call that has not returned is described rather than waited on for
/// good — `JoinHandle::join` is an unbounded blocking receive, and taking it on
/// a handle nothing will complete turns this diagnosis into the hang it exists
/// to prevent.
///
/// The report is waited for rather than sampled once. A `try_recv` that found
/// nothing said "the serve call is still running", which is one of two states
/// and not the likelier one: `camber::spawn` hands back a refused handle when
/// the scope is already closed, the closure never runs, and nothing ever sends —
/// so the sentence described a serve call that never started as one in
/// progress. What is claimed on expiry is only what is known: no return within
/// the bound.
fn unready_cause(
    served: JoinHandle<Result<(), RuntimeError>>,
    exited: &Receiver<()>,
    unready: &std::io::Error,
) -> String {
    match exited.recv_timeout(EXIT_REPORT_BOUND) {
        Ok(()) => format!(
            "the fixture server never answered within {READINESS_BOUND:?}: {unready}; \
             serving had already returned {:?}",
            served.join()
        ),
        Err(_) => format!(
            "the fixture server never answered within {READINESS_BOUND:?}: {unready}; \
             the serve call has not returned within {EXIT_REPORT_BOUND:?}, so it \
             either never started or is still running"
        ),
    }
}

pub fn spawn_server(router: Router) -> SocketAddr {
    spawn_bound(move |listener| http::serve_listener(listener, router))
}

/// Serve a host router on an ephemeral port and wait until it answers.
///
/// [`spawn_server`]'s counterpart for the cases that turn on the authority a
/// peer sent. Stated once here because host resolution, host mapper precedence,
/// and routing-stage middleware order are proved in three different test
/// binaries, and a copy of the spawn in each is a copy that can drift.
pub fn spawn_host_server(hosts: HostRouter) -> SocketAddr {
    spawn_bound(move |listener| http::serve_hosts(listener, hosts))
}
