//! Bounded streaming uploads, read off the wire a peer actually sent.
//!
//! Every claim here is about one race and its consequences: a request's own
//! bound against the answer its upstream gives, settled before anything is
//! committed to the downstream peer. What each branch owes is different — one
//! refuses and forwards nothing more, the other stops the upload and carries
//! the answer out over payload nobody will read — so both are driven through a
//! real client, a real proxy, and a real local upstream.

use crate::common;
use crate::http as wire;

use camber::RuntimeError;
use camber::http::mock::{
    InboundTerminal, RequestBodyOwnerController, ResponseCommit, ResponseCommitmentEdge,
    ResponseOrigin, ScopedAdmittedCommitment,
};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, HostRouter, Next, RejectionKind, Request, RequestBodyMode,
    Response, Router,
};
use camber::runtime;
use common::{Journal, RawUpstream, UpstreamAnswers, only, raw_upstream, recording_mapper};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The prefix every streaming route here forwards from.
const PREFIX: &str = "/api";
/// The path a case asks for behind that prefix.
const TARGET: &str = "/api/echo";
/// The route pattern a request behind the prefix matches.
const PREFIX_ROUTE: &str = "/api/*proxy_path";
/// The configured ceiling every streaming route here runs under.
const CEILING: usize = 64;
/// The body a raw upstream answers a forwarded request with.
const UPSTREAM_BODY: &str = "upstream-answered";
/// How long an observation has to settle.
const SETTLE_BOUND: Duration = Duration::from_secs(5);
/// A payload frame that fits inside every ceiling here.
const UNDER: &[u8] = b"under-the-bound";
/// A payload frame that crosses every ceiling here.
const CROSSING: [u8; CEILING] = [b'x'; CEILING];
/// A backend address nothing in this binary can be listening on.
const DEAD_BACKEND: &str = "http://127.0.0.1:1";

/// Read one journal past a poisoned lock.
///
/// A case that panicked while holding a journal poisons it, and the assertion
/// that would report the failure is then the one that panics on the lock
/// instead. Every read below states that recovery through here.
fn read_journal<T>(journal: &Mutex<Vec<T>>) -> std::sync::MutexGuard<'_, Vec<T>> {
    journal.lock().unwrap_or_else(|error| error.into_inner())
}

/// What one streaming route's policy, mapper, and permit reported.
struct Probes {
    calls: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    outstanding: Arc<AtomicUsize>,
    mapped: Journal,
}

impl Probes {
    fn new() -> Self {
        Self::sharing(&Arc::new(AtomicUsize::new(0)))
    }

    /// Probes for one route, drawing its permits from a pool it may share.
    fn sharing(pool: &Arc<AtomicUsize>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            drops: Arc::new(AtomicUsize::new(0)),
            outstanding: Arc::clone(pool),
            mapped: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }

    fn mapped_count(&self) -> usize {
        read_journal(&self.mapped).len()
    }
}

/// A policy that admits under `limit` and hands the request a permit.
fn permitting_policy(
    limit: usize,
    probes: &Probes,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(&probes.calls);
    let drops = Arc::clone(&probes.drops);
    let pool = Arc::clone(&probes.outstanding);
    move |_context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(BodyAdmission::with_permit(
            limit,
            wire::pooled_permit_probe(&drops, &pool),
        ))
    }
}

/// Serve one streaming proxy on an observed listener, under one admission policy.
fn streaming_proxy(
    backend: &str,
    selected: usize,
    probes: &Probes,
) -> wire::ObservedServer<ScopedAdmittedCommitment> {
    let port = wire::reserve_admitted_commitment();
    let mut router = Router::new();
    router.proxy_stream(PREFIX, backend);
    port.serve(
        router
            .max_request_body(CEILING)
            .body_admission(permitting_policy(selected, probes))
            .rejection_mapper(recording_mapper(&probes.mapped, "proxy")),
    )
}

/// Open one chunked upload to `host` on a connection the case then paces.
///
/// The body is deliberately left unterminated: what these cases observe is what
/// happens partway through an upload, and a peer that had already finished would
/// leave nothing for either branch of the race to stop. A head that never
/// reached the proxy is the fixture's own transport breaking, and the case that
/// asked for it has nothing left to assert.
fn open_upload(peer: &mut std::net::TcpStream, host: &str) {
    wire::write_chunked_head(peer, wire::KEEP_CONNECTION, "POST", TARGET, host)
        .expect("the upload head reached the proxy");
}

// ── An upstream head that wins stops the upload before it commits ────

#[test]
fn upstream_head_winner_quiesces_upload_before_downstream_commit() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnHead);
            let probes = Probes::new();
            let server = streaming_proxy(&upstream.backend(), CEILING, &probes);
            let controller = server.controller();
            controller
                .commitment
                .pause_once(ResponseCommitmentEdge::BeforeDownstreamCommit)
                .expect("arm the streaming commitment checkpoint");

            let mut peer = wire::connect(server.addr()).expect("the paced peer connected");
            open_upload(&mut peer, wire::DEFAULT_HOST);
            wire::tolerate_dead_socket(wire::write_chunk(&mut peer, UNDER))
                .expect("the first frame reached the proxy");

            wire::wait_paused_blocking(
                controller,
                ResponseCommitmentEdge::BeforeDownstreamCommit,
                "the answer reached its commitment checkpoint",
            );

            let quiesced = wire::body_owners(server.controller()).frames_polled;
            assert!(
                quiesced >= 1,
                "the frames sent before the answer were polled and counted"
            );
            assert_eq!(
                probes.drops(),
                1,
                "the upload released its permit before the answer was committed"
            );
            assert_eq!(
                wire::body_owners(server.controller()).permit_owners_dropped,
                1,
                "the listener counted the same single release"
            );
            assert_eq!(
                probes.mapped_count(),
                0,
                "an answer taken over a stopped upload maps no refusal"
            );

            // Sent after quiescence and before the commitment is released: the
            // upload has stopped polling, so this frame can reach no counter.
            // The write is the whole of what the frame counter below is measured
            // against, so a frame that never left this process is a failure
            // rather than a claim about a frame nothing sent.
            wire::tolerate_dead_socket(wire::write_chunk(&mut peer, b"after-quiescence"))
                .expect("the frame sent after quiescence reached the proxy");
            controller
                .commitment
                .release(ResponseCommitmentEdge::BeforeDownstreamCommit)
                .expect("release the answer onto the wire");

            let answered = wire::read_http_response_bounded(&mut peer)
                .expect("the upstream's answer reached the peer");
            assert_eq!(answered.status, 200, "the upstream's own status is carried");
            assert_eq!(answered.text().as_ref(), UPSTREAM_BODY);
            assert_eq!(
                answered.header("connection").map(str::to_ascii_lowercase),
                Some("close".to_owned()),
                "an answer over unread payload ends the connection"
            );
            assert_eq!(
                wire::body_owners(server.controller()).frames_polled,
                quiesced,
                "no frame is polled after the upload has quiesced"
            );
            wire::assert_connection_closed(&mut peer, "head winner");

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Committed barriers, not a rank, decide the bound against the head ─

/// How many unordered iterations the race row runs.
///
/// Each is a complete exchange against a live proxy and a live upstream, so what
/// the count buys is sensitivity to a result outside the documented closed set
/// rather than a winner. The listener is served once and every iteration is
/// measured as a step above its own baseline, so the count costs one exchange
/// each rather than a bind, a serve, and a joined teardown.
const RACE_ITERATIONS: usize = 32;

/// What one row's peer read, and what its refusal journal held.
struct Answered {
    status: u16,
    mapped: Box<[common::Observed]>,
}

/// Open one upload, send a frame that fits, and hand back the paced peer.
///
/// The frame tolerates a peer that has already been answered. A row holding one
/// producer at its own edge cannot reach that state, so it loses nothing; the
/// unordered row can, because an upstream head committed over an undrained
/// upload ends the connection and the frame then meets a socket the proxy has
/// closed. That is the answer arriving early, and the closed result set is what
/// reads it.
fn opened_upload<Owner>(server: &wire::ObservedServer<Owner>) -> std::net::TcpStream {
    let mut peer = wire::connect(server.addr()).expect("the racing peer connected");
    open_upload(&mut peer, wire::DEFAULT_HOST);
    wire::tolerate_answered_peer(wire::write_chunk(&mut peer, UNDER))
        .expect("the first frame reached the proxy, or the proxy had already answered");
    peer
}

/// Read one row's answer and the refusals its route's mapper was handed.
fn answered(peer: &mut std::net::TcpStream, probes: &Probes) -> Answered {
    let answered = wire::read_http_response_bounded(peer).expect("the row was answered");
    Answered {
        status: answered.status,
        mapped: common::drain(&probes.mapped),
    }
}

/// What the release counters one row is measured against already stood at.
///
/// Both counters are cumulative per listener, and the race row drives every one
/// of its iterations against a single served listener. Taken before that row's
/// peer opens, this baseline turns each cumulative counter back into the one
/// release the iteration itself owed. A row that serves its own listener takes
/// the same reading and finds it at zero.
struct Cleanup {
    drops: usize,
    owners: usize,
}

impl Cleanup {
    /// Read what this row's two release counters stand at right now.
    fn take<Owner: wire::Owns<RequestBodyOwnerController>>(
        server: &wire::ObservedServer<Owner>,
        probes: &Probes,
    ) -> Self {
        Self {
            drops: probes.drops(),
            owners: wire::body_owners(server.controller()).permit_owners_dropped,
        }
    }
}

/// Assert one row left the cleanup every result here owes, whichever it was.
///
/// Identical for both allowed results, which is the point: a race whose two
/// members owed different cleanup would be two behaviours rather than one
/// contract with two valid answers.
///
/// Both counts are read as one more than `before` rather than as one outright.
/// The claim is the same either way — this row released exactly what it took —
/// and stating it as a step lets a row that shares its listener with the
/// iterations around it make the claim its own.
fn assert_row_cleanup<Owner: wire::Owns<RequestBodyOwnerController>>(
    peer: &mut std::net::TcpStream,
    server: &wire::ObservedServer<Owner>,
    probes: &Probes,
    upstream: &RawUpstream,
    before: &Cleanup,
    row: &str,
) {
    wire::assert_connection_closed(peer, row);
    assert!(
        !upstream.forwarded(&CROSSING),
        "{row}: no crossing frame reaches an upstream"
    );
    wire::assert_released(&probes.drops, before.drops + 1, row);
    let counted = wire::poll_until(SETTLE_BOUND, || {
        wire::body_owners(server.controller()).permit_owners_dropped == before.owners + 1
    });
    assert!(
        counted,
        "{row}: the listener counted the same single release"
    );
}

/// Row A: the upstream head reaches its commit barrier before the crossing is
/// released, so the upstream status is what the peer reads.
///
/// The barrier is production's own: the coordinator has taken this operation's
/// commitment for the upstream head and quiesced the upload by the time it is
/// held here. Only then does this row put the crossing frame on the wire, so the
/// crossing meets a commitment another producer already holds and maps nothing.
fn assert_head_committed_first(upstream: &RawUpstream, probes: &Probes) {
    let row = "upstream head committed first";
    let server = streaming_proxy(&upstream.backend(), CEILING, probes);
    let controller = server.controller();
    let committed = ResponseCommitmentEdge::BeforeDownstreamCommit;
    controller
        .commitment
        .pause_once(committed)
        .expect("arm the downstream commit edge");

    let before = Cleanup::take(&server, probes);
    let mut peer = opened_upload(&server);
    wire::wait_paused_blocking(controller, committed, row);

    let observed = controller.commitment.observed();
    assert_eq!(
        observed.commits, 1,
        "{row}: the upstream head took the one commitment"
    );
    wire::tolerate_dead_socket(wire::write_chunk(&mut peer, &CROSSING))
        .expect("the crossing frame reached the proxy");
    controller
        .commitment
        .release(committed)
        .expect("let the committed head go downstream");

    let read = answered(&mut peer, probes);
    assert_eq!(
        read.status, 200,
        "{row}: a head committed before the crossing was released stays authoritative"
    );
    assert!(
        read.mapped.is_empty(),
        "{row}: a crossing that reached a taken commitment maps nothing"
    );
    assert_row_cleanup(&mut peer, &server, probes, upstream, &before, row);
}

/// Row B: the crossing commits before the upstream head is allowed through, so
/// the peer reads this route's mapped refusal.
fn assert_crossing_committed_first(upstream: &RawUpstream, probes: &Probes) {
    let row = "crossing committed first";
    let server = streaming_proxy(&upstream.backend(), CEILING, probes);
    let controller = server.controller();
    let head = ResponseCommitmentEdge::UpstreamHeadReady;
    controller
        .commitment
        .pause_once(head)
        .expect("arm the upstream-head edge");

    let before = Cleanup::take(&server, probes);
    let mut peer = opened_upload(&server);
    wire::wait_paused_blocking(controller, head, row);
    wire::tolerate_dead_socket(wire::write_chunk(&mut peer, &CROSSING))
        .expect("the crossing frame reached the proxy");

    let read = answered(&mut peer, probes);
    assert_eq!(
        read.status, 413,
        "{row}: a crossing committed before the head was released owns the answer"
    );
    let seen = common::only_of(&read.mapped, row);
    assert_eq!(seen.kind, RejectionKind::BodyLimit);
    assert_eq!(seen.route.as_deref(), Some(PREFIX_ROUTE));
    assert_eq!(
        seen.raw_path.as_ref(),
        TARGET,
        "{row}: the refusal names the request its own classification selected"
    );
    common::assert_request_id_shape(Some(&seen.request_id), row);

    controller
        .commitment
        .release(head)
        .expect("let the losing head go");
    assert_row_cleanup(&mut peer, &server, probes, upstream, &before, row);
}

/// Row C: both producers are released with no edge between them.
///
/// Either result is correct, and the closed set is the whole claim: this route's
/// mapped `BodyLimit` or the status the upstream committed. Every iteration owes
/// the same cleanup whichever it got, which is what makes the two members one
/// contract rather than two behaviours.
fn assert_unordered_race(upstream: &RawUpstream) {
    // One listener and one set of probes for every iteration. What each row owed
    // is read as a step above the baseline it took first, so a later iteration
    // cannot inherit an earlier one's single release and pass on it — which is
    // the whole reason the counters were once served fresh.
    let probes = &Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, probes);
    for iteration in 0..RACE_ITERATIONS {
        let row = format!("unordered race {iteration}");
        let before = Cleanup::take(&server, probes);
        let commits_before = server.controller().commitment.observed().commits;
        let mut peer = opened_upload(&server);
        // No barrier holds this row's producers apart, so the upstream head can
        // already have been committed, written, and its connection ended. The
        // crossing frame then reaches a closed socket, which is that member of
        // the closed set winning rather than the fixture's transport breaking.
        wire::tolerate_answered_peer(wire::write_chunk(&mut peer, &CROSSING))
            .expect("the crossing frame reached the proxy, or the proxy had already answered");

        let read = answered(&mut peer, probes);
        // Each member is read off the production cell as well as off the wire.
        // A status alone leaves the set open: a `200` some other producer
        // committed reads the same on this socket, and the closed set would
        // have silently gained a third member.
        let committed = server.controller().commitment.observed();
        match read.status {
            413 => {
                let seen = common::only_of(&read.mapped, &row);
                assert_eq!(seen.kind, RejectionKind::BodyLimit);
                assert_eq!(seen.route.as_deref(), Some(PREFIX_ROUTE));
                assert_eq!(
                    committed.committed,
                    Some(ResponseCommit::Cause(InboundTerminal::RouteBodyLimit)),
                    "{row}: the mapped refusal is the fact this operation committed: {committed:?}"
                );
            }
            200 => {
                assert!(
                    read.mapped.is_empty(),
                    "{row}: a committed upstream status maps no refusal"
                );
                assert_eq!(
                    committed.committed,
                    Some(ResponseCommit::Head(ResponseOrigin::Upstream)),
                    "{row}: the upstream head is the fact this operation committed: {committed:?}"
                );
            }
            other => panic!("{row}: {other} is outside the documented closed result set"),
        }
        assert_eq!(
            committed.commits,
            commits_before + 1,
            "{row}: whichever member won, one producer took the commitment: {committed:?}"
        );
        assert_row_cleanup(&mut peer, &server, probes, upstream, &before, &row);
    }
}

/// Invariant 11
///
/// A streaming upload's own bound and its upstream's head are two producers of
/// one answer. Neither outranks the other: whichever reaches this operation's
/// response commitment first owns the response, and the two rows with a real
/// barrier between them prove both directions of that. The third row releases
/// them with no ordering edge at all and accepts only the documented closed set,
/// requiring the identical cleanup for both members.
#[test]
fn streaming_body_limit_and_upstream_head_follow_committed_barriers() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnHead);
            assert_head_committed_first(&upstream, &Probes::new());
            assert_crossing_committed_first(&upstream, &Probes::new());
            assert_unordered_race(&upstream);

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── One admitted permit, one release, on every terminal path ─────────

/// Forward a complete upload, and prove a drained one leaves its connection.
///
/// Both halves are the same terminal. An upload that drained left nothing framed
/// behind the answer, so the connection is still the peer's to use — and that is
/// the whole observable difference between this ending and the stopped one an
/// early upstream head produces. Without it, a proxy that ended every streamed
/// answer's connection would satisfy every other claim here.
fn assert_completion_releases_once_and_keeps_the_connection() {
    let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnBodyEnd);
    let probes = Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

    let (answered, mut peer) = wire::send_chunked(
        server.addr(),
        wire::KEEP_CONNECTION,
        "POST",
        TARGET,
        wire::DEFAULT_HOST,
        &[UNDER],
    )
    .expect("the complete upload was answered");
    assert_eq!(answered.status, 200, "a complete upload reaches upstream");
    assert_eq!(answered.text().as_ref(), UPSTREAM_BODY);
    assert_eq!(
        answered.header("connection").map(str::to_ascii_lowercase),
        None,
        "a drained upload leaves no unread payload to end the connection for"
    );
    assert!(
        upstream.forwarded(UNDER),
        "every admitted frame reached the upstream"
    );
    wire::assert_released(&probes.drops, 1, "streaming completion");
    assert_eq!(
        wire::body_owners(server.controller()).permit_owners_dropped,
        1
    );

    let framed = wire::probe_connection_reuse(&mut peer, "POST", TARGET, &[], UNDER)
        .expect("the reuse probe reached the same connection")
        .expect("a drained upload leaves the connection able to frame another request");
    assert_eq!(framed.status, 200, "the second request was answered");
    assert_eq!(framed.text().as_ref(), UPSTREAM_BODY);
    wire::assert_released(&probes.drops, 2, "streaming completion reuse");
}

/// Cross the bound and prove the same single release.
fn assert_observed_limit_releases_once() {
    let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnBodyEnd);
    let probes = Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

    let (refused, _peer) = wire::send_chunked(
        server.addr(),
        wire::KEEP_CONNECTION,
        "POST",
        TARGET,
        wire::DEFAULT_HOST,
        &[UNDER, &CROSSING],
    )
    .expect("the crossing upload was answered");
    assert_eq!(refused.status, 413, "the crossing frame is refused");
    wire::assert_released(&probes.drops, 1, "observed limit");
}

/// Take an early upstream head and prove the stopped upload released once.
fn assert_early_head_releases_once() {
    let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnHead);
    let probes = Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

    let mut peer = wire::connect(server.addr()).expect("the early-head peer connected");
    open_upload(&mut peer, wire::DEFAULT_HOST);
    wire::tolerate_dead_socket(wire::write_chunk(&mut peer, UNDER))
        .expect("the frame reached the proxy");
    let answered =
        wire::read_http_response_bounded(&mut peer).expect("the early head reached the peer");
    assert_eq!(answered.status, 200, "the early upstream head is committed");
    wire::assert_released(&probes.drops, 1, "early upstream head");
}

/// Disconnect while the upload is still active.
fn assert_disconnect_releases_once() {
    let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnBodyEnd);
    let probes = Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

    let mut peer = wire::connect(server.addr()).expect("the disconnecting peer connected");
    open_upload(&mut peer, wire::DEFAULT_HOST);
    wire::tolerate_dead_socket(wire::write_chunk(&mut peer, UNDER))
        .expect("the frame reached the proxy");
    drop(peer);
    wire::assert_released(&probes.drops, 1, "disconnect during upload");
    assert_eq!(
        wire::body_owners(server.controller()).permit_owners_dropped,
        1
    );
}

/// Cancel the request future while it waits on an upstream that never answers.
fn assert_cancellation_releases_once() {
    let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::Withheld);
    let probes = Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

    let mut peer = wire::connect(server.addr()).expect("the cancelling peer connected");
    open_upload(&mut peer, wire::DEFAULT_HOST);
    wire::tolerate_dead_socket(wire::write_chunk(&mut peer, UNDER))
        .expect("the frame reached the proxy");
    wire::tolerate_dead_socket(wire::write_chunked_end(&mut peer))
        .expect("the body terminator reached the proxy");
    let reached = wire::poll_until(SETTLE_BOUND, || upstream.connections() == 1);
    assert!(
        reached,
        "the upload reached an upstream that withholds its answer"
    );
    drop(peer);
    wire::assert_released(&probes.drops, 1, "request-future cancellation");
}

/// Fail the upstream leg outright and prove the same single release.
fn assert_upstream_failure_releases_once() {
    let probes = Probes::new();
    // Port one, which no ephemeral allocation ever hands out and nothing in this
    // binary can bind. A port freed by dropping its own listener is a port the
    // next concurrent bind may take, and the refusal this row reads would then be
    // some other fixture answering.
    let server = streaming_proxy(DEAD_BACKEND, CEILING, &probes);

    let (refused, _peer) = wire::send_chunked(
        server.addr(),
        wire::CLOSE_AFTER_RESPONSE,
        "POST",
        TARGET,
        wire::DEFAULT_HOST,
        &[UNDER],
    )
    .expect("the unreachable upstream was answered for");
    assert_eq!(
        refused.status, 502,
        "an unreachable upstream is a bad gateway"
    );
    let seen = only(&probes.mapped, "upstream transport failure");
    assert_eq!(seen.kind, RejectionKind::Proxy);
    wire::assert_released(&probes.drops, 1, "upstream transport failure");
}

#[test]
fn streaming_permit_releases_once_on_completion_disconnect_and_cancellation() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            assert_completion_releases_once_and_keeps_the_connection();
            assert_observed_limit_releases_once();
            assert_early_head_releases_once();
            assert_disconnect_releases_once();
            assert_cancellation_releases_once();
            assert_upstream_failure_releases_once();
            runtime::request_shutdown();
        })
        .unwrap();
}

// ── A failed upload transport is not a limit and not a proxy fault ───

#[test]
fn streaming_body_transport_failure_remains_body_unreadable_before_commit() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::Withheld);
            let probes = Probes::new();
            let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

            let mut peer = wire::connect(server.addr()).expect("the failing peer connected");
            open_upload(&mut peer, wire::DEFAULT_HOST);
            wire::tolerate_dead_socket(wire::write_chunk(&mut peer, UNDER))
                .expect("the first frame reached the proxy");
            // Not a chunk size. The framing under the payload is broken, which
            // is the transport's fault and not this request's size. It is also
            // the whole act this case is about, so a write that never left this
            // process is a failure rather than a refusal to attribute.
            use std::io::Write;
            wire::tolerate_dead_socket(
                peer.write_all(b"not-a-size\r\n")
                    .and_then(|()| peer.flush()),
            )
            .expect("the broken framing reached the proxy");

            let refused = wire::read_http_response_bounded(&mut peer)
                .expect("the broken framing was answered");
            assert_eq!(
                refused.status, 400,
                "an unreadable body is the peer's fault"
            );
            let seen = only(&probes.mapped, "streaming transport failure");
            assert_eq!(
                seen.kind,
                RejectionKind::BodyUnreadable,
                "a failed upload transport is neither a limit nor a proxy fault"
            );
            assert_eq!(seen.route.as_deref(), Some(PREFIX_ROUTE));
            let reached = wire::poll_until(SETTLE_BOUND, || upstream.connections() == 1);
            assert!(
                reached,
                "the upload opened a real upstream leg before its transport failed"
            );
            assert_eq!(
                upstream.answered(),
                0,
                "no upstream head was committed for a request that never finished arriving"
            );
            // The upstream withholds its answer, so it is still holding this
            // leg open: the only thing that ends it is the proxy dropping it.
            let cancelled = wire::poll_until(SETTLE_BOUND, || upstream.dropped() == 1);
            assert!(
                cancelled,
                "the upstream leg was cancelled rather than left waiting on a body that stopped"
            );
            wire::assert_released(&probes.drops, 1, "streaming transport failure");
            wire::assert_connection_closed(&mut peer, "streaming transport failure");

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Admission runs before the gate and before the upstream ───────────

/// What one streaming policy call saw before the first payload frame.
struct SeenContext {
    request_id: Box<str>,
    method: Box<str>,
    raw_path: Box<str>,
    route: Box<str>,
    streaming: bool,
    declared: Option<u64>,
    header: Option<Box<str>>,
    polled: usize,
}

type ContextJournal = Arc<Mutex<Vec<SeenContext>>>;

/// A policy that journals its context and then answers as `admit` says.
fn journalling_policy(
    seen: &ContextJournal,
    probes: &Probes,
    controller: &Arc<ScopedAdmittedCommitment>,
    admit: bool,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let seen = Arc::clone(seen);
    let calls = Arc::clone(&probes.calls);
    let drops = Arc::clone(&probes.drops);
    let pool = Arc::clone(&probes.outstanding);
    let controller = Arc::clone(controller);
    move |context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        read_journal(&seen).push(SeenContext {
            request_id: context.request_id().as_str().into(),
            method: context.method().into(),
            raw_path: context.raw_path().into(),
            route: context.route().into(),
            streaming: context.mode() == RequestBodyMode::Streaming,
            declared: context.declared_length(),
            header: context.header("X-CaSe").map(Into::into),
            polled: controller.bodies.observed().frames_polled,
        });
        match admit {
            true => Ok(BodyAdmission::with_permit(
                CEILING,
                wire::pooled_permit_probe(&drops, &pool),
            )),
            false => Err(RuntimeError::InvalidArgument(
                "the streaming policy declined this work".into(),
            )),
        }
    }
}

/// Assert the one journalled context this row produced.
///
/// `mapped` is the identity the route's own mapper recorded, when a mapper ran
/// for this row: the claim is that admission and mapping named the same
/// request. A gate refusal produces no mapped rejection, so that row checks the
/// shape of the identity admission was given and makes no identity claim at all
/// — comparing the journalled identity to itself would have read as one.
fn assert_streaming_context(seen: &ContextJournal, mapped: Option<&str>, label: &str) {
    let journal = read_journal(seen);
    let entry = journal.last().expect("the policy journalled its context");
    common::assert_request_id_shape(Some(&entry.request_id), label);
    match mapped {
        Some(mapped) => assert_eq!(
            mapped,
            entry.request_id.as_ref(),
            "{label}: admission and mapping named one request"
        ),
        None => {}
    }
    assert_eq!(entry.method.as_ref(), "POST", "{label}: method");
    assert_eq!(entry.raw_path.as_ref(), TARGET, "{label}: raw path");
    assert_eq!(entry.route.as_ref(), PREFIX_ROUTE, "{label}: route");
    assert!(entry.streaming, "{label}: streaming mode");
    assert_eq!(
        entry.declared,
        Some(UNDER.len() as u64),
        "{label}: declared"
    );
    assert_eq!(
        entry.header.as_deref(),
        Some("seen"),
        "{label}: first case-insensitive header value"
    );
    assert_eq!(entry.polled, 0, "{label}: no frame polled before admission");
}

/// Serve a streaming proxy whose gate reports every call it receives.
fn gated_proxy(
    backend: &str,
    seen: &ContextJournal,
    probes: &Probes,
    admit: bool,
    gate: &Arc<AtomicUsize>,
) -> wire::ObservedServer<ScopedAdmittedCommitment> {
    let port = wire::reserve_admitted_commitment();
    let controller = port.controller();
    let mut router = Router::new();
    router.proxy_stream(PREFIX, backend);
    let gate = Arc::clone(gate);
    router.use_middleware(move |_request: &Request, _next: Next<'_>| {
        let gate = Arc::clone(&gate);
        async move {
            gate.fetch_add(1, Ordering::SeqCst);
            Response::text(403, "gate refused").expect("valid gate status")
        }
    });
    port.serve(
        router
            .max_request_body(CEILING)
            .body_admission(journalling_policy(seen, probes, &controller, admit))
            .rejection_mapper(recording_mapper(&probes.mapped, "proxy")),
    )
}

/// Send one declared upload to a gated proxy and read the answer it earns.
///
/// The connection comes back with the answer, because what a refusal decided
/// over payload it never read is half of one row's claim. The peer offers to
/// keep it, so a connection that cannot frame another request was ended by the
/// framework rather than by the preference it was sent.
fn upload_to_gate(addr: std::net::SocketAddr) -> (wire::HttpResponse, std::net::TcpStream) {
    let mut peer = wire::connect(addr).expect("the gated peer connected");
    wire::write_request_with_connection(
        &mut peer,
        wire::KEEP_CONNECTION,
        "POST",
        TARGET,
        &[("x-case", "seen")],
        UNDER,
    )
    .expect("the gated upload reached the proxy");
    let answered =
        wire::read_http_response_bounded(&mut peer).expect("the gated upload was answered");
    (answered, peer)
}

/// Assert a declined policy stops the request before its gate and its upstream.
fn assert_declined_policy_stops_first(upstream: &RawUpstream) {
    let probes = Probes::new();
    let seen_context: ContextJournal = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(AtomicUsize::new(0));
    let server = gated_proxy(&upstream.backend(), &seen_context, &probes, false, &gate);

    let (refused, _peer) = upload_to_gate(server.addr());
    assert_eq!(refused.status, 503, "a declined policy refuses the work");
    let seen = only(&probes.mapped, "policy decline");
    assert_eq!(seen.kind, RejectionKind::BodyAdmission);
    assert_streaming_context(&seen_context, Some(&seen.request_id), "policy decline");
    assert_eq!(probes.calls(), 1, "the policy ran once");
    assert_eq!(
        gate.load(Ordering::SeqCst),
        0,
        "a declined policy leaves the specialized gate unrun"
    );
    assert_eq!(
        upstream.connections(),
        0,
        "a declined policy reaches no upstream"
    );
    assert_eq!(wire::body_owners(server.controller()).frames_polled, 0);
    assert_eq!(probes.drops(), 0, "a declined policy hands over no permit");
}

/// Assert an admitted request still stops at its gate, releasing its permit and
/// ending the connection over the payload nobody read.
///
/// One case for both halves of what a gate refusal after admission owes. Two
/// fixtures proved these four things from separately hand-built routers, and the
/// only thing either had that the other lacked was the disposition on one side
/// and the journalled context on the other.
fn assert_gate_refuses_after_admission(upstream: &RawUpstream) {
    let probes = Probes::new();
    let seen_context: ContextJournal = Arc::new(Mutex::new(Vec::new()));
    let gate = Arc::new(AtomicUsize::new(0));
    let server = gated_proxy(&upstream.backend(), &seen_context, &probes, true, &gate);

    let (blocked, mut peer) = upload_to_gate(server.addr());
    assert_eq!(blocked.status, 403, "the gate refuses after admission");
    assert_streaming_context(&seen_context, None, "gate refusal");
    assert_eq!(probes.calls(), 1, "the policy ran once");
    assert_eq!(
        probes.mapped_count(),
        0,
        "a gate's own response is not a refusal, so it reaches no mapper at all"
    );
    assert_eq!(
        gate.load(Ordering::SeqCst),
        1,
        "the gate ran once, after the permit was granted"
    );
    assert_eq!(
        wire::body_owners(server.controller()).frames_polled,
        0,
        "a refused gate polls no payload frame"
    );
    assert_eq!(
        upstream.connections(),
        0,
        "a refused gate reaches no upstream"
    );
    assert_eq!(
        blocked.header("connection").map(str::to_ascii_lowercase),
        Some("close".to_owned()),
        "a gate refusal over unread payload ends the connection"
    );
    wire::assert_connection_closed(&mut peer, "gate refusal after admission");
    wire::assert_released(&probes.drops, 1, "gate refusal after admission");
}

/// Assert the same upstream does take a handoff once nothing refuses first.
///
/// The two rows above read a connection count of zero off this fixture, and a
/// fixture that counted nothing at all would satisfy both. This row is what
/// makes that zero a number the upstream could have moved.
fn assert_admitted_request_reaches_upstream(upstream: &RawUpstream) {
    let probes = Probes::new();
    let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

    let (answered, _peer) = wire::send_chunked(
        server.addr(),
        wire::CLOSE_AFTER_RESPONSE,
        "POST",
        TARGET,
        wire::DEFAULT_HOST,
        &[UNDER],
    )
    .expect("the ungated upload was answered");
    assert_eq!(answered.status, 200, "an admitted upload reaches upstream");
    assert_eq!(
        upstream.connections(),
        1,
        "the handoff the refused rows never made is the one this row counts"
    );
    assert!(
        upstream.forwarded(UNDER),
        "the admitted frame reached the upstream"
    );
    wire::assert_released(&probes.drops, 1, "admitted streaming handoff");
}

#[test]
fn streaming_admission_precedes_gate_and_upstream_handoff() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnBodyEnd);
            assert_declined_policy_stops_first(&upstream);
            assert_gate_refuses_after_admission(&upstream);
            assert_admitted_request_reaches_upstream(&upstream);
            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Host routing selects the child's own policy and ceiling ──────────

/// The host whose child configures no ceiling of its own.
const INHERITING_HOST: &str = "inherit.test";
/// The host whose child narrows the ceiling it is contained by.
const NARROWING_HOST: &str = "narrow.test";
/// The ceiling the host router configures over both children.
const HOST_CEILING: usize = 48;
/// The ceiling the narrowing child configures for itself.
const CHILD_CEILING: usize = 24;
/// The maximum the narrowing child's own policy then selects.
const CHILD_SELECTED: usize = 12;
/// The cap a configured ceiling above it is clamped to.
const HARD_MAX: usize = 256 * 1024 * 1024;

/// Build one host child under `backend`, with its own policy and mapper.
fn host_child(backend: &str, ceiling: Option<usize>, selected: usize, probes: &Probes) -> Router {
    let mut router = Router::new();
    router.proxy_stream(PREFIX, backend);
    let configured = match ceiling {
        Some(bytes) => router.max_request_body(bytes),
        None => router,
    };
    configured
        .body_admission(permitting_policy(selected, probes))
        .rejection_mapper(recording_mapper(&probes.mapped, "child"))
}

/// Send one complete chunked upload to `host` and read the answer it earns.
///
/// The connection is not kept: every caller here reads one answer and asserts on
/// it, and the dispositions this file claims are read on the paced connections
/// above.
fn upload_to_host(addr: std::net::SocketAddr, host: &str, chunks: &[&[u8]]) -> wire::HttpResponse {
    let (answered, _peer) =
        wire::send_chunked(addr, wire::KEEP_CONNECTION, "POST", TARGET, host, chunks)
            .expect("the host upload was answered");
    answered
}

/// Prove a local limit remains authoritative when aborting its outbound body.
#[test]
fn streaming_limit_outranks_the_upstream_failure_it_causes() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnBodyEnd);
            let probes = Probes::new();
            let server = streaming_proxy(&upstream.backend(), CEILING, &probes);

            for attempt in 1..=32 {
                let answered =
                    upload_to_host(server.addr(), wire::DEFAULT_HOST, &[UNDER, &CROSSING]);
                assert_eq!(
                    answered.status, 413,
                    "attempt {attempt}: the local refusal outranks its derived outbound error"
                );
            }
            assert_eq!(probes.mapped_count(), 32, "every refusal maps once");
            wire::assert_released(&probes.drops, 32, "derived upstream failures");

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Assert the narrowing child's own selected maximum bounds what it forwards.
///
/// Twelve bytes, which is narrower than that child's own ceiling and narrower
/// again than the host's: what the request is measured against is the last
/// number in the chain, not the first.
fn assert_narrowing_child(addr: std::net::SocketAddr, upstream: &RawUpstream, narrowing: &Probes) {
    let over_selected = [b'n'; CHILD_SELECTED + 1];
    let narrowed = upload_to_host(addr, NARROWING_HOST, &[&over_selected]);
    assert_eq!(
        narrowed.status, 413,
        "the selected maximum narrows what the child may forward"
    );
    let seen = only(&narrowing.mapped, "narrowing child");
    assert_eq!(seen.kind, RejectionKind::BodyLimit);
    common::assert_request_id_shape(Some(&seen.request_id), "narrowing child");
    assert_eq!(
        seen.origin, "child",
        "the selected child's own mapper answered"
    );

    let under_selected = [b'n'; CHILD_SELECTED];
    let admitted = upload_to_host(addr, NARROWING_HOST, &[&under_selected]);
    assert_eq!(admitted.status, 200, "the selected maximum still admits");
    assert!(upstream.forwarded(&under_selected));
    // Read behind the admitted frame, not before it. The upstream serves one
    // connection to its end before accepting the next, so a byte it recorded
    // from this second upload is recorded after everything the refused one ever
    // forwarded — while a read taken straight after the refusal reads whatever
    // the upstream thread happened to have drained by then, and a proxy that did
    // forward the crossing frame passes whenever that thread is a poll behind.
    assert!(
        !upstream.forwarded(&over_selected),
        "the crossing frame reaches no upstream, in a read ordered behind one that did"
    );
    wire::assert_released(&narrowing.drops, 2, "narrowing child");
}

/// Assert the hard cap narrows a streaming route that configured past it.
///
/// Declared rather than sent: what the row claims is the ceiling, and a quarter
/// of a gigabyte of payload would prove the same number at the cost of moving
/// it. The route's own policy asks for every byte and its configured ceiling
/// asks for every byte, so the cap is the only thing left that can refuse.
fn assert_hard_cap_narrows(upstream: &RawUpstream) {
    let probes = Probes::new();
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.proxy_stream(PREFIX, &upstream.backend());
    let server = port.serve(
        router
            .max_request_body(usize::MAX)
            .body_admission(permitting_policy(usize::MAX, &probes))
            .rejection_mapper(recording_mapper(&probes.mapped, "hard-cap")),
    );
    let opened = upstream.connections();

    let refused = wire::send_to_host_with(
        server.addr(),
        "POST",
        TARGET,
        "localhost",
        &[("Content-Length", &(HARD_MAX + 1).to_string())],
    );
    assert_eq!(
        refused.status, 413,
        "a configured ceiling above the hard cap is clamped to it"
    );
    let seen = only(&probes.mapped, "hard cap");
    assert_eq!(seen.kind, RejectionKind::BodyLimit);
    assert_eq!(seen.origin, "hard-cap", "the route's own mapper answered");
    assert_eq!(
        upstream.connections(),
        opened,
        "a declaration above the hard cap reaches no upstream"
    );
    assert_eq!(
        wire::body_owners(server.controller()).frames_polled,
        0,
        "a declaration refused before arrival polls no frame"
    );

    let admitted = upload_to_host(server.addr(), "localhost", &[UNDER]);
    assert_eq!(
        admitted.status, 200,
        "the same route still forwards what the cap admits"
    );
    assert!(upstream.forwarded(UNDER));
    wire::assert_released(&probes.drops, 1, "hard cap");
}

#[test]
fn host_routed_streaming_policy_and_ceiling_select_child() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream = raw_upstream(200, UPSTREAM_BODY, UpstreamAnswers::OnBodyEnd);
            // One pool behind both children: a per-child release count cannot
            // say that everything one pool issued came back.
            let pool = Arc::new(AtomicUsize::new(0));
            let inheriting = Probes::sharing(&pool);
            let narrowing = Probes::sharing(&pool);
            let port = wire::reserve_request_body_owner();
            let mut hosts = HostRouter::new();
            hosts.add(
                INHERITING_HOST,
                host_child(&upstream.backend(), None, usize::MAX, &inheriting),
            );
            hosts.add(
                NARROWING_HOST,
                host_child(
                    &upstream.backend(),
                    Some(CHILD_CEILING),
                    CHILD_SELECTED,
                    &narrowing,
                ),
            );
            let server = port.serve_hosts(hosts.max_request_body(HOST_CEILING));

            // The inheriting child has no ceiling of its own and a policy that
            // asks for every byte, so what bounds it is the host's own ceiling.
            let over_host = [b'h'; HOST_CEILING + 1];
            let refused = upload_to_host(server.addr(), INHERITING_HOST, &[&over_host]);
            assert_eq!(
                refused.status, 413,
                "a child that configured nothing cannot exceed the host ceiling"
            );
            let seen = only(&inheriting.mapped, "inheriting child");
            assert_eq!(seen.kind, RejectionKind::BodyLimit);
            assert_eq!(narrowing.calls(), 0, "only the selected child's policy ran");

            let under_host = [b'h'; HOST_CEILING - 1];
            let forwarded = upload_to_host(server.addr(), INHERITING_HOST, &[&under_host]);
            assert_eq!(
                forwarded.status, 200,
                "a payload inside the host ceiling is forwarded"
            );
            assert!(upstream.forwarded(&under_host));
            // Ordered behind the admitted frame for the reason
            // [`assert_narrowing_child`] states: the upstream serves one
            // connection to its end before the next, so this read follows
            // everything the refused upload could ever have forwarded.
            assert!(
                !upstream.forwarded(&over_host),
                "the crossing frame reaches no upstream, in a read ordered behind one that did"
            );
            wire::assert_released(&inheriting.drops, 2, "inheriting child");

            assert_narrowing_child(server.addr(), &upstream, &narrowing);
            assert_eq!(
                inheriting.calls(),
                2,
                "each request reached exactly one child policy"
            );
            let returned = wire::poll_until(SETTLE_BOUND, || pool.load(Ordering::SeqCst) == 0);
            assert!(
                returned,
                "every permit the shared pool issued was released: {} outstanding",
                pool.load(Ordering::SeqCst)
            );

            assert_hard_cap_narrows(&upstream);

            runtime::request_shutdown();
        })
        .unwrap();
}
