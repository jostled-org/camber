//! What operators see while a served process runs one bounded operation.
//!
//! 10.T2 owns the profiling worker. Sampling and rendering run on a blocking
//! thread that is not the Tokio worker awaiting them, the rendered answer is
//! retained under the one maximum the serving policy froze, and the write that
//! crosses that maximum is dropped, answered as a redacted internal-service
//! failure, and recorded for operators under the bound it crossed.

#[cfg(feature = "profiling")]
use crate::common;
#[cfg(feature = "profiling")]
use crate::http as http_support;

#[cfg(feature = "profiling")]
use camber::http::mock::{self, LifecycleCheckpoint, LifecycleController};
#[cfg(feature = "profiling")]
use camber::http::{Request, Response, Router, ServerPolicy};
#[cfg(feature = "profiling")]
use camber::runtime;
#[cfg(feature = "profiling")]
use std::net::SocketAddr;
#[cfg(feature = "profiling")]
use std::time::Duration;

/// The target every profiling row asks for, sampling for one real second.
#[cfg(feature = "profiling")]
const PROFILING_TARGET: &str = "/debug/pprof/cpu?seconds=1";

/// The recorded field that selects this route's operator events.
#[cfg(feature = "profiling")]
const PROFILING_PATH_FIELD: &str = "raw_path=/debug/pprof/cpu";

/// The route a row asks for while a profiler entry is held.
#[cfg(feature = "profiling")]
const HELD_WITNESS_TARGET: &str = "/quick";

/// How many busy threads give the sampler stacks to render.
#[cfg(feature = "profiling")]
const LOAD_THREADS: usize = 2;

/// The maximum an explicit opt-out reports having frozen.
///
/// Production measures every retained answer against one number, and the
/// opt-out's number is the largest total a collection could reach. Naming it
/// here is what lets a row say "this render froze no maximum" as a value.
#[cfg(feature = "profiling")]
const UNBOUNDED: usize = usize::MAX;

/// How long a bounded fixture teardown may take.
#[cfg(feature = "profiling")]
const SHUTDOWN_BOUND: Duration = Duration::from_secs(5);

/// How long a live peer waits for an answer its server has to sample for.
#[cfg(feature = "profiling")]
const ANSWER_BOUND: Duration = Duration::from_secs(60);

/// The typed cause an operator reads when a rendered profile crosses its bound.
#[cfg(feature = "profiling")]
const PROFILING_CEILING_CAUSE: &str = "cause=byte limit exceeded: profiling_response";

/// The category that crossing is recorded under.
///
/// A profile the service cannot answer with is the operator's configuration,
/// so the refusal is Camber's and not the peer's request.
#[cfg(feature = "profiling")]
const PROFILING_CROSSING_KIND: &str = "kind=internal_service";

/// What the production owners published while one profiling request ran.
///
/// Every number is written by the owner that decided it: the blocking worker
/// that entered and returned, and the checked collector that accounted for each
/// write before retaining it. Nothing here chooses a maximum, retains a byte, or
/// selects the thread anything runs on.
#[cfg(feature = "profiling")]
struct Rendered {
    /// The maximum this render is measured against, as its collector compares it.
    ceiling: usize,
    /// What the first accounted write left retained.
    first_write: usize,
    /// The most this render ever held at once.
    peak: usize,
    /// How many writes the renderer handed the capped owner.
    writes: usize,
}

#[cfg(feature = "profiling")]
impl Rendered {
    fn of(controller: &LifecycleController) -> Self {
        Self {
            ceiling: controller.profiling_observed().frozen_ceiling,
            first_write: controller.collected_first_retained_bytes(),
            peak: controller.collected_peak_retained_bytes(),
            writes: controller.collected_chunks_polled(),
        }
    }
}

/// Assert one worker entered, returned, and never ran on the thread awaiting it.
///
/// The fixture runtime has exactly one worker and the request awaiting this
/// worker runs on it, so "not the awaiting thread" is "no Tokio worker at all".
#[cfg(feature = "profiling")]
fn assert_one_worker_ran_off_the_only_tokio_worker(controller: &LifecycleController, label: &str) {
    let observed = controller.profiling_observed();
    assert_eq!(observed.workers_entered, 1, "{label}: workers entered");
    assert_eq!(observed.workers_returned, 1, "{label}: workers returned");
    assert_eq!(
        observed.entries_on_caller, 0,
        "{label}: sampling and rendering must not run on the worker awaiting them",
    );
}

/// The policy every profiling row serves under, before it names its ceiling.
#[cfg(feature = "profiling")]
fn base_policy() -> ServerPolicy {
    ServerPolicy::default()
        .shutdown_timeout(SHUTDOWN_BOUND)
        .expect("the row's shutdown deadline")
}

/// The routes a profiling row is served beside.
///
/// One ordinary route, for the request a row sends while the profiler entry is
/// held: the built-in profiling path is matched before dispatch, so a row needs
/// a registered route to prove the server still answers anything at all.
#[cfg(feature = "profiling")]
fn served_routes() -> Router {
    let mut router = Router::new();
    router.get(HELD_WITNESS_TARGET, |_req: &Request| async {
        Response::text(200, "quick")
    });
    router
}

/// Serve one profiling row under `policy` and hand back its address.
#[cfg(feature = "profiling")]
fn serve(policy: ServerPolicy) -> http_support::ObservedServer {
    http_support::reserve_observed().serve_with_policy(served_routes(), policy)
}

/// The whole opt-out row, and the exact boundary the crossing row then uses.
///
/// The worker is held at its entry, before it has sampled anything. While it is
/// held, the one Tokio worker this fixture has answers an ordinary request — so
/// the sampling window that follows is provably not standing on it. Released,
/// the render is retained whole because this server named the opt-out and the
/// runtime containing it named the same.
///
/// Returns what the first accounted write left retained, which is the exact
/// maximum the crossing row freezes.
#[cfg(feature = "profiling")]
async fn assert_held_entry_renders_whole_under_the_opt_out() -> usize {
    let label = "the opt-out render";
    let controller = mock::profiling_lifecycle().expect("one profiling observer");
    let server = serve(base_policy().unbounded_profiling_response());
    let addr = server.addr();
    controller
        .pause_once(LifecycleCheckpoint::ProfilingWorkerEntered)
        .expect("one armed profiling worker checkpoint");

    let profiling = tokio::spawn(profile(addr));
    controller
        .wait_until_paused(LifecycleCheckpoint::ProfilingWorkerEntered)
        .await
        .expect("the profiling worker entered");

    let held = controller.profiling_observed();
    assert_eq!(held.workers_entered, 1, "{label}: the worker entered");
    assert_eq!(
        held.workers_returned, 0,
        "{label}: the held worker still owns its answer",
    );
    assert_eq!(
        Rendered::of(&controller).peak,
        0,
        "{label}: nothing is retained before the sampler runs",
    );
    let witness = camber::http::get(&format!("http://{addr}{HELD_WITNESS_TARGET}"))
        .await
        .expect("the request sent while the profiler is held was answered");
    assert_eq!(
        witness.status(),
        200,
        "{label}: a held profiler entry occupies no Tokio worker",
    );

    controller
        .release(LifecycleCheckpoint::ProfilingWorkerEntered)
        .expect("the held worker resumes");
    let answered = profiling
        .await
        .expect("the profiling request task")
        .expect("the profiling answer");
    assert_eq!(answered.status(), 200, "{label}: unexpected status");
    assert!(
        answered.body().starts_with("<?xml"),
        "{label}: the whole rendered profile reaches the peer",
    );

    let rendered = Rendered::of(&controller);
    assert_eq!(
        rendered.ceiling, UNBOUNDED,
        "{label}: only the named opt-out renders under no maximum at all",
    );
    assert_eq!(
        rendered.peak,
        answered.body_bytes().len(),
        "{label}: the answer the peer received is what the render retained",
    );
    assert!(
        rendered.writes > 1,
        "{label}: the renderer writes incrementally, so a bound applies per write",
    );
    assert!(
        rendered.first_write > 0 && rendered.first_write < rendered.peak,
        "{label}: the first write is the exact boundary the crossing row freezes, \
         and it is only part of the answer: {} of {}",
        rendered.first_write,
        rendered.peak,
    );
    assert_one_worker_ran_off_the_only_tokio_worker(&controller, label);

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the opt-out fixture tore down");
    rendered.first_write
}

/// The crossing row: a maximum exactly one write wide.
///
/// The renderer's first write is a fixed prologue, so freezing the maximum at its
/// size is what makes both halves of the accounting rule observable in one live
/// render: the write that lands exactly on the maximum is kept, and the write
/// after it crosses and is dropped. The peer is told only that the service
/// failed; the operator is told which bound the render crossed.
///
/// The policy is the caller's, because two rows resolve this same maximum two
/// ways: one server names it, and one server opts out under a runtime that names
/// it. Both must render under it.
#[cfg(feature = "profiling")]
async fn assert_the_crossing_write_is_dropped_and_named(
    policy: ServerPolicy,
    ceiling: usize,
    label: &str,
) {
    let controller = mock::profiling_lifecycle().expect("one profiling observer");
    let server = serve(policy);
    let addr = server.addr();

    let captured = common::capture_events(PROFILING_PATH_FIELD);
    let answered = profile(addr)
        .await
        .expect("the refused profiling request was answered");

    assert_eq!(answered.status(), 500, "{label}: unexpected status");
    assert_eq!(
        answered.body(),
        common::REDACTED_BODY,
        "{label}: the peer is told only that the service failed",
    );
    assert!(
        !answered.body().contains("<?xml") && !answered.body().contains("svg"),
        "{label}: no rendered byte reaches the peer: {}",
        answered.body(),
    );

    let rendered = Rendered::of(&controller);
    assert_eq!(
        rendered.ceiling, ceiling,
        "{label}: the render is measured against the maximum this server froze",
    );
    assert_eq!(
        rendered.peak, ceiling,
        "{label}: the write that lands exactly on the maximum is kept whole",
    );
    assert_eq!(
        rendered.first_write, ceiling,
        "{label}: the exact-limit write is the first one the renderer made",
    );
    assert!(
        rendered.writes > 1,
        "{label}: the write past the maximum reached the collector and was dropped",
    );
    assert_one_worker_ran_off_the_only_tokio_worker(&controller, label);

    // The peer learned nothing. The operator learned which maximum was crossed,
    // which is the only place that typed provenance can be held to.
    let events = captured.events();
    let recorded = common::only_event(&events, common::REJECTION_MESSAGE, label);
    common::assert_fields(
        recorded,
        &[PROFILING_CEILING_CAUSE, PROFILING_CROSSING_KIND],
        label,
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the crossing fixture tore down");
}

/// One live profiling request, bounded by the peer's own deadline.
#[cfg(feature = "profiling")]
async fn profile(addr: SocketAddr) -> Result<Response, camber::RuntimeError> {
    camber::http::client()
        .request_timeout(ANSWER_BOUND)
        .get(&format!("http://{addr}{PROFILING_TARGET}"))
        .await
}

/// One runtime that serves profiling requests under `containing` as its ceiling.
///
/// Exactly one worker, so "not the thread awaiting this worker" is "no Tokio
/// worker at all": the request that awaits the profiling worker runs on the only
/// worker there is.
#[cfg(feature = "profiling")]
fn profiling_runtime(containing: ServerPolicy) -> camber::runtime::RuntimeBuilder {
    common::test_runtime()
        .worker_threads(1)
        .with_profiling()
        .server_policy(containing)
}

/// 10.T2
#[cfg(feature = "profiling")]
#[test]
fn profiling_sampling_and_rendering_are_bounded_off_workers() {
    // The load is owned here, outside both runtimes, so it is running before the
    // first sampling window opens and is stopped and joined after the last one
    // closes — including when a row unwinds.
    // Dropping the load stops and joins it too, which is what covers a row that
    // unwinds; `stop` is the success path, where the join is asserted.
    let load = common::CpuLoad::start(LOAD_THREADS);
    let boundary = profiling_runtime(base_policy().unbounded_profiling_response())
        .run(|| {
            let boundary = common::block_on(async {
                let boundary = assert_held_entry_renders_whole_under_the_opt_out().await;
                assert_the_crossing_write_is_dropped_and_named(
                    base_policy()
                        .profiling_response_limit(boundary)
                        .expect("a finite profiling ceiling"),
                    boundary,
                    "the crossing render",
                )
                .await;
                boundary
            });
            runtime::request_shutdown();
            boundary
        })
        .expect("the opt-out fixture runtime ran to completion");

    // The containing policy is resolved when a server starts inside a runtime, so
    // naming a finite outer maximum takes a second runtime. The server inside this
    // one opts out and renders under the runtime's maximum anyway: an inner
    // opt-out inherits an outer bound rather than erasing it.
    profiling_runtime(
        base_policy()
            .profiling_response_limit(boundary)
            .expect("a finite containing ceiling"),
    )
    .run(|| {
        common::block_on(assert_the_crossing_write_is_dropped_and_named(
            base_policy().unbounded_profiling_response(),
            boundary,
            "the contained opt-out",
        ));
        runtime::request_shutdown();
    })
    .expect("the containing fixture runtime ran to completion");

    load.stop();
}
