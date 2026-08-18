//! What operators see while a served process runs one bounded operation.
//!
//! 10.T2 owns the profiling worker. Sampling and rendering run on a blocking
//! thread that is not the Tokio worker awaiting them, the rendered answer is
//! retained under the one maximum the serving policy froze, and the write that
//! crosses that maximum is dropped, answered as a redacted internal-service
//! failure, and recorded for operators under the bound it crossed.
//!
//! Every spelling of that maximum is served here — the unnamed one, the named
//! finite one, and the opt-out — because a rendered profile this suite produces
//! is far under the documented default, so nothing on the wire tells a defaulted
//! server from one that froze no maximum at all.
//!
//! 15.T1 and 15.T2 own what one admitted operation is finally recorded as. The
//! record is written where the operation actually ends — the body or the
//! handoff — under the status the peer was given, the whole head-to-terminal
//! span, and the highest-precedence typed boundary any owner fixed. Its labels
//! carry only closed vocabularies production itself publishes.

use crate::common;
use crate::http as http_support;

#[cfg(feature = "profiling")]
use camber::__private::DEFAULT_PROFILING_RESPONSE_LIMIT;
#[cfg(feature = "profiling")]
use camber::http::mock::{self, LifecycleCheckpoint, LifecycleController};
use camber::http::{Request, Response};
#[cfg(feature = "profiling")]
use camber::http::{Router, ServerPolicy};
use camber::runtime;
#[cfg(feature = "profiling")]
use std::net::SocketAddr;
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

/// The unnamed spelling is not a spelling of unbounded.
///
/// A profile rendered from this fixture's load is orders of magnitude under
/// eight MiB, so what a defaulted server answers with is byte for byte what the
/// opt-out row beside it answers with: a bound assertion on the body would hold
/// with no ceiling wired at all. The two spellings are told apart by the maximum
/// production froze — this server names no profiling spelling and renders under
/// the documented default, and only the named opt-out renders under none.
#[cfg(feature = "profiling")]
async fn assert_the_unnamed_spelling_freezes_the_documented_maximum() {
    let label = "the defaulted render";
    let controller = mock::profiling_lifecycle().expect("one profiling observer");
    let server = serve(base_policy());
    let addr = server.addr();

    let answered = profile(addr)
        .await
        .expect("the defaulted profiling request was answered");
    assert_eq!(answered.status(), 200, "{label}: unexpected status");
    assert!(
        answered.body().starts_with("<?xml"),
        "{label}: the whole rendered profile reaches the peer",
    );

    let rendered = Rendered::of(&controller);
    assert_eq!(
        rendered.ceiling, DEFAULT_PROFILING_RESPONSE_LIMIT,
        "{label}: the unnamed spelling renders under the documented maximum",
    );
    assert_eq!(
        rendered.peak,
        answered.body_bytes().len(),
        "{label}: the answer the peer received is what the render retained",
    );
    assert!(
        rendered.peak < rendered.ceiling,
        "{label}: this answer is far inside the maximum, so only the frozen \
         number distinguishes it from the opt-out: {} of {}",
        rendered.peak,
        rendered.ceiling,
    );
    assert_one_worker_ran_off_the_only_tokio_worker(&controller, label);

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the defaulted fixture tore down");
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
                assert_the_unnamed_spelling_freezes_the_documented_maximum().await;
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

// ---------------------------------------------------------------------------
// 15.T1 / 15.T2: one record, at the terminal the operation actually reached
// ---------------------------------------------------------------------------

/// The counter one completed operation is recorded under.
const COMPLETION_METRIC: &str = "http_requests_total";

/// The duration family's own count series, read beside the counter.
///
/// Both instruments carry one label set, so a row that moved one and not the
/// other would be describing two different requests.
const DURATION_COUNT_METRIC: &str = "http_request_duration_seconds_count";

/// The sentence one completed operation is recorded under.
const COMPLETION_EVENT: &str = "message=request completed";

/// The sentence one refused operation is recorded under.
const REJECTION_EVENT: &str = "message=request rejected";

/// The field a refusal states its private cause in.
const CAUSE_FIELD: &str = "cause=";

/// How long any completion row's exchange, read, or rendezvous may take.
const COMPLETION_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

/// The request lifetime the completion fixture freezes.
///
/// Far above what every other row spends and far below the bound above, so the
/// one row that never produces a head is the only row that reaches it.
const COMPLETION_TOTAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The request payload maximum the completion fixture freezes.
const COMPLETION_MAX_BODY: usize = 16;

/// The routes the completion rows are served on.
///
/// Each names its own class, because the record a row reads is selected by the
/// dispatch class label and two rows sharing a class would read one counter.
const BUFFERED_PATH: &str = "/completion/buffered";
const STREAM_PATH: &str = "/completion/stream";
const SSE_PATH: &str = "/completion/sse";
const PROXY_PREFIX: &str = "/completion/proxy";
const PROXY_PATH: &str = "/completion/proxy/held";
const HELD_UPSTREAM_PATH: &str = "/held";
const ABSENT_PATH: &str = "/completion/absent";
#[cfg(feature = "ws")]
const UPGRADE_PATH: &str = "/completion/upgrade";
const LIMITED_PATH: &str = "/completion/limited";
const UNANSWERED_PATH: &str = "/completion/unanswered";
const DEAD_PREFIX: &str = "/completion/dead";
const DEAD_PATH: &str = "/completion/dead/echo";

/// A backend nothing listens on.
///
/// The row that forwards here is refused with the upstream's own diagnostic,
/// which is text production holds while it records and a label must never
/// carry.
const DEAD_BACKEND: &str = "http://127.0.0.1:1";

/// The methods the ordinary-HTTP rows drive.
///
/// Deliberately not `GET`: this binary's other metrics journey drives `GET` and
/// `HEAD` ordinary-HTTP rows against the same process-wide registry, and a row
/// that shared a whole label set with one of them would read a counter two
/// fixtures move.
const BUFFERED_METHOD: &str = "PATCH";
const ABSENT_METHOD: &str = "DELETE";
const LIMITED_METHOD: &str = "PUT";
const UNANSWERED_METHOD: &str = "POST";

/// The method the failing-upstream row forwards with.
///
/// `GET` is safe here where the ordinary-HTTP rows above could not use it: this
/// row is answered on the proxy class under a refusal status, so its whole
/// label set is shared with nothing else this process records.
const DEAD_METHOD: &str = "GET";

/// What one row must have been recorded as, once.
struct Expected<'a> {
    label: &'a str,
    method: &'a str,
    status: u16,
    protocol: &'a str,
    terminal: &'a str,
    boundary: &'a str,
}

impl Expected<'_> {
    /// The whole label set this row's record carries.
    ///
    /// Built as one value because it is one claim: a delta taken over a subset
    /// of the labels would count a record another class produced.
    fn labels<'a>(&'a self, status: &'a str) -> [(&'a str, &'a str); 5] {
        [
            ("method", self.method),
            ("status", status),
            ("protocol", self.protocol),
            ("terminal", self.terminal),
            ("boundary", self.boundary),
        ]
    }
}

/// What one scrape reports about completed operations.
///
/// The counter and the duration family are read out of one body, so the two are
/// one moment rather than two: a second send would itself be a completed
/// operation between them.
struct Recorded {
    completions: Box<[common::Sample]>,
    durations: Box<[common::Sample]>,
}

impl Recorded {
    fn scraped(addr: std::net::SocketAddr) -> Self {
        let scrape = http_support::send(addr, "GET", "/metrics", &[], b"");
        Self {
            completions: common::scraped_samples(&scrape, COMPLETION_METRIC),
            durations: common::scraped_samples(&scrape, DURATION_COUNT_METRIC),
        }
    }
}

/// How far both instruments moved for one row's whole label set.
///
/// Stated as one number because the two have to agree: a counter that moved
/// while the duration family did not is a completion recorded under a label set
/// no operator can read a latency for.
fn moved(before: &Recorded, after: &Recorded, expected: &Expected<'_>) -> u64 {
    let status = expected.status.to_string();
    let labels = expected.labels(&status);
    let counted = common::delta(&before.completions, &after.completions, &labels);
    let timed = common::delta(&before.durations, &after.durations, &labels);
    assert_eq!(
        counted, timed,
        "{}: the counter and the duration family disagree about {labels:?}",
        expected.label,
    );
    counted
}

/// Assert one row was recorded exactly once, under everything it declared.
fn assert_recorded_once(before: &Recorded, after: &Recorded, expected: &Expected<'_>) {
    assert_eq!(
        moved(before, after, expected),
        1,
        "{}: exactly one record was expected at this row's terminal",
        expected.label,
    );
}

/// Assert nothing was recorded for this row yet.
fn assert_not_recorded_yet(before: &Recorded, held: &Recorded, expected: &Expected<'_>) {
    assert_eq!(
        moved(before, held, expected),
        0,
        "{}: a record was written before this operation reached its terminal",
        expected.label,
    );
}

/// The one completion event this row's path produced.
fn only_completion(capture: &common::TraceCapture, label: &str) -> Box<str> {
    let events = capture.events();
    common::only_event(&events, COMPLETION_EVENT, label).into()
}

/// Assert one completion event names this row's status, class, and boundary.
fn assert_event_matches(event: &str, expected: &Expected<'_>) {
    let stated = [
        format!("status={}", expected.status),
        format!("protocol={}", expected.protocol),
        format!("terminal={}", expected.terminal),
        format!("boundary={}", expected.boundary),
    ];
    let stated: Box<[&str]> = stated.iter().map(String::as_str).collect();
    common::assert_fields(event, &stated, expected.label);
}

/// How long one recorded operation reports having taken.
fn recorded_latency(event: &str, label: &str) -> u128 {
    common::field_value(event, "latency_ms")
        .unwrap_or_else(|| panic!("{label}: the completion event reports no latency"))
        .parse()
        .unwrap_or_else(|error| panic!("{label}: the recorded latency is unreadable: {error}"))
}

/// A producer the fixture releases, and the release the row holds.
///
/// The channel is the rendezvous: the row reads its head, states that nothing
/// has been recorded, and only then lets the body end. Nothing here decides a
/// terminal — the production body still does — but the moment it reaches one is
/// the row's to choose, which is what makes absence-before-terminal an
/// assertion rather than a race.
struct Release(tokio::sync::mpsc::UnboundedSender<()>);

impl Release {
    fn release(&self, label: &str) {
        self.0
            .send(())
            .unwrap_or_else(|_| panic!("{label}: the held producer is still listening"));
    }
}

/// Register a streaming route whose one frame waits for its release.
fn register_held_stream(router: &mut camber::http::Router, path: &'static str) -> Release {
    let (release, releases) = tokio::sync::mpsc::unbounded_channel::<()>();
    let releases = std::sync::Arc::new(tokio::sync::Mutex::new(releases));
    router.get_stream(path, move |_req: &Request| {
        let releases = std::sync::Arc::clone(&releases);
        Box::pin(async move {
            let (response, sender) = camber::http::StreamResponse::new(200);
            tokio::spawn(async move {
                let held = releases.lock().await.recv().await;
                match held {
                    Some(()) => drop(sender.send("released").await),
                    None => {}
                }
            });
            response.with_header("Content-Type", "text/x-held")
        })
    });
    Release(release)
}

/// Register an SSE feed whose one event waits for its release.
///
/// The producer is a blocking callback, so its wait is a blocking receive under
/// the suite bound: a feed left waiting past it fails its own row rather than
/// holding a worker for the rest of the binary.
fn register_held_sse(router: &mut camber::http::Router, path: &'static str) -> Release {
    let (release, releases) = tokio::sync::mpsc::unbounded_channel::<()>();
    let releases = std::sync::Mutex::new(releases);
    router.get_sse(
        path,
        move |_req: &Request, writer: &mut camber::http::SseWriter| {
            let held = releases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .blocking_recv();
            match held {
                Some(()) => writer.event("message", "released"),
                None => Ok(()),
            }
        },
    );
    Release(release)
}

/// What production handed the fixture's own mapper.
///
/// 15.T2 reads its forbidden values off this rather than writing them down
/// beside it. A diagnostic or a peer address the test invented is a string
/// production never held, and searching labels for one of those passes whatever
/// the recorder does; a string the recorder was holding when it wrote its
/// labels is the only kind this claim can be made about.
#[derive(Default)]
struct MapperRecord {
    held: std::sync::Mutex<Vec<Box<str>>>,
}

impl MapperRecord {
    /// Keep what one refusal was handed, exactly as production spelled it.
    fn observe(&self, diagnostic: &str, peer: Option<std::net::IpAddr>) {
        let mut held = self.held.lock().unwrap_or_else(|error| error.into_inner());
        held.push(diagnostic.into());
        match peer {
            Some(peer) => held.push(peer.to_string().into()),
            None => {}
        }
    }

    /// Everything production has handed this mapper so far.
    fn taken(&self) -> Box<[Box<str>]> {
        self.held
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect()
    }
}

/// The mapper the completion fixture states its own detail through.
///
/// It answers under the status its producer chose, so every row that declared
/// an answer keeps it, and it names the request the way Camber's own built-in
/// answer would, because a configured policy owns every header of what it
/// returns. What it adds is one operator-only detail beside the diagnostic it
/// was handed — two values production is holding at the moment it records.
fn detailing_mapper(
    record: &std::sync::Arc<MapperRecord>,
) -> impl Fn(
    &camber::http::Rejection,
    &camber::http::RejectionContext,
) -> Result<Response, camber::RuntimeError>
+ Send
+ Sync
+ 'static {
    let record = std::sync::Arc::clone(record);
    move |rejection: &camber::http::Rejection, context: &camber::http::RejectionContext| {
        record.observe(rejection.message(), context.remote_addr());
        common::naming(
            Response::text(
                rejection.status(),
                &format!("{MAPPER_DETAIL}: {}", rejection.message()),
            ),
            context,
        )
    }
}

/// The whole completion service: every class, and the upstream one of them
/// forwards to.
struct CompletionFixture {
    server: http_support::ObservedServer,
    upstream: http_support::ObservedServer,
    stream: Release,
    sse: Release,
    upstream_stream: Release,
    mapper: std::sync::Arc<MapperRecord>,
}

impl CompletionFixture {
    fn serve() -> Self {
        let mut upstream_routes = camber::http::Router::new();
        let upstream_stream = register_held_stream(&mut upstream_routes, HELD_UPSTREAM_PATH);
        let upstream = http_support::reserve_observed().serve(upstream_routes);

        let mapper = std::sync::Arc::new(MapperRecord::default());
        let mut routes = camber::http::Router::new();
        routes.patch(BUFFERED_PATH, |_req: &Request| async {
            Response::text(200, "buffered")
        });
        let stream = register_held_stream(&mut routes, STREAM_PATH);
        let sse = register_held_sse(&mut routes, SSE_PATH);
        routes.proxy_stream(PROXY_PREFIX, &format!("http://{}", upstream.addr()));
        // Nothing listens on this backend, so the row that forwards here is
        // refused with the upstream's own diagnostic and the fixture's mapper
        // states its detail over it.
        routes.proxy_stream(DEAD_PREFIX, DEAD_BACKEND);
        routes.put(LIMITED_PATH, |_req: &Request| async {
            Response::text(200, "admitted")
        });
        // Nothing ever resolves this handler, so the one row that drives it
        // reaches the request lifetime the fixture froze and nothing else does.
        routes.post(UNANSWERED_PATH, |_req: &Request| async {
            std::future::pending::<()>().await;
            Response::text(200, "unreachable")
        });
        register_completion_grpc(&mut routes);
        register_completion_upgrade(&mut routes);

        let routes = routes
            .max_request_body(COMPLETION_MAX_BODY)
            .request_budget(
                camber::http::RequestBudget::unbounded()
                    .with_total(COMPLETION_TOTAL)
                    .expect("the fixture's request lifetime is accepted"),
            )
            .rejection_mapper(detailing_mapper(&mapper));
        Self {
            server: http_support::reserve_observed().serve(routes),
            upstream,
            stream,
            sse,
            upstream_stream,
            mapper,
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.server.addr()
    }

    /// Stop both owners in bounded order.
    fn tear_down(self, label: &str) {
        for (owner, name) in [(self.server, "completion"), (self.upstream, "upstream")] {
            owner
                .shutdown_bounded(SHUTDOWN_BOUND)
                .unwrap_or_else(|error| {
                    panic!("{label}: the {name} owner did not end gracefully: {error}")
                });
        }
    }
}

/// The gRPC service the completion matrix's own class is answered by.
///
/// The whole generated module is included, and this root drives only the unary
/// service in it through Camber's own registration. The wrapper's `serve`
/// constructor is generated all the same, so its unused spelling is allowed
/// here rather than in the codegen.
#[cfg(feature = "grpc")]
#[allow(dead_code)]
mod completion_proto {
    tonic::include_proto!("greeter");
}

#[cfg(feature = "grpc")]
struct CompletionGreeter;

#[cfg(feature = "grpc")]
#[tonic::async_trait]
impl completion_proto::greeter_server::Greeter for CompletionGreeter {
    async fn say_hello(
        &self,
        request: tonic::Request<completion_proto::HelloRequest>,
    ) -> Result<tonic::Response<completion_proto::HelloReply>, tonic::Status> {
        Ok(tonic::Response::new(completion_proto::HelloReply {
            message: format!("Hello, {}!", request.into_inner().name),
        }))
    }
}

/// Register the unary service the gRPC completion row drives.
#[cfg(feature = "grpc")]
fn register_completion_grpc(router: &mut camber::http::Router) {
    router.grpc(camber::http::GrpcRouter::new().add_service(
        completion_proto::greeter_server::GreeterServer::new(CompletionGreeter),
    ));
}

#[cfg(not(feature = "grpc"))]
fn register_completion_grpc(_router: &mut camber::http::Router) {}

/// Register the upgrade the handoff completion row commits.
#[cfg(feature = "ws")]
fn register_completion_upgrade(router: &mut camber::http::Router) {
    router.ws(UPGRADE_PATH, |_req: &Request, _ws: camber::http::WsConn| {
        Ok(())
    });
}

#[cfg(not(feature = "ws"))]
fn register_completion_upgrade(_router: &mut camber::http::Router) {}

/// 15.T1: a committed `101` is recorded at the handoff, not at a body.
///
/// An upgrade has no body to end. Its HTTP operation ends the moment the
/// transport passes to the WebSocket subsystem, and the session past that point
/// reports through its own terminals rather than through this record.
#[cfg(feature = "ws")]
fn assert_upgrade_records_at_its_handoff(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "upgrade",
        method: "GET",
        status: 101,
        protocol: "websocket",
        terminal: "completed",
        boundary: "none",
    };
    let before = Recorded::scraped(fixture.addr());
    let head = common::ws_upgrade_request_to("localhost", UPGRADE_PATH);
    let mut peer = common::start_upgrade_with(fixture.addr(), &head);
    let answered = common::read_until_double_crlf(&mut peer);
    assert!(
        answered.starts_with("HTTP/1.1 101"),
        "upgrade: the handshake never committed: {answered}",
    );
    drop(peer);

    let settled = http_support::poll_until(COMPLETION_BOUND, || {
        moved(&before, &Recorded::scraped(fixture.addr()), &expected) >= 1
    });
    assert!(settled, "upgrade: the committed handoff was never recorded");
    assert_recorded_once(&before, &Recorded::scraped(fixture.addr()), &expected);
}

#[cfg(not(feature = "ws"))]
fn assert_upgrade_records_at_its_handoff(_fixture: &CompletionFixture) {}

/// Drive one whole exchange under the suite's wire bound.
fn exchange(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
) -> http_support::HttpResponse {
    http_support::send(addr, method, path, &[], body)
}

/// Open one peer, send its request, and hand back the socket with the head read.
///
/// The split is the point: a class whose body the fixture holds has committed
/// its head and produced nothing else, which is exactly the moment a record
/// must not exist yet.
fn peer_reading_head(addr: std::net::SocketAddr, path: &str, label: &str) -> std::net::TcpStream {
    let mut peer = http_support::connect(addr)
        .unwrap_or_else(|error| panic!("{label}: the peer could not connect: {error}"));
    http_support::write_request(&mut peer, "GET", path, &[], b"")
        .unwrap_or_else(|error| panic!("{label}: the peer could not send: {error}"));
    let head = http_support::read_head(&mut peer, COMPLETION_BOUND)
        .unwrap_or_else(|error| panic!("{label}: no head arrived: {error}"));
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{label}: the held class never committed its head: {head}",
    );
    peer
}

/// 15.T1: a buffered answer is recorded once, at the body Hyper wrote.
fn assert_buffered_records_once(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "buffered",
        method: BUFFERED_METHOD,
        status: 200,
        protocol: "ordinary_http",
        terminal: "completed",
        boundary: "none",
    };
    let capture = common::capture_events(&format!("path={BUFFERED_PATH}"));
    let before = Recorded::scraped(fixture.addr());
    let answered = exchange(fixture.addr(), BUFFERED_METHOD, BUFFERED_PATH, b"ok");
    assert_eq!(answered.status, 200, "buffered: wire status");
    let after = Recorded::scraped(fixture.addr());
    assert_recorded_once(&before, &after, &expected);
    assert_event_matches(&only_completion(&capture, "buffered"), &expected);
}

/// 15.T1: a held class is recorded only once its own body ends.
///
/// One shape for the three classes that own a body Camber produces, because the
/// claim is the same for all three and only the release differs: the head is on
/// the wire, nothing is recorded, the fixture lets the body end, and exactly one
/// record appears naming the whole span.
fn assert_held_class_records_at_its_body(
    fixture: &CompletionFixture,
    path: &'static str,
    release: &Release,
    expected: &Expected<'_>,
) {
    let capture = common::capture_events(&format!("path={path}"));
    let before = Recorded::scraped(fixture.addr());
    let opened = std::time::Instant::now();
    let mut peer = peer_reading_head(fixture.addr(), path, expected.label);

    let held = Recorded::scraped(fixture.addr());
    assert_not_recorded_yet(&before, &held, expected);
    // Held long enough that the recorded span cannot be the head's alone: the
    // record has to cover the body this row deliberately kept open.
    let holding = std::time::Duration::from_millis(80);
    std::thread::sleep(holding);

    release.release(expected.label);
    http_support::read_until_closed(&mut peer, COMPLETION_BOUND)
        .unwrap_or_else(|error| panic!("{}: the held body never ended: {error}", expected.label));
    drop(peer);

    let settled = http_support::poll_until(COMPLETION_BOUND, || {
        moved(&before, &Recorded::scraped(fixture.addr()), expected) >= 1
    });
    assert!(
        settled,
        "{}: no record appeared after the body reached its terminal",
        expected.label,
    );
    let after = Recorded::scraped(fixture.addr());
    assert_recorded_once(&before, &after, expected);

    let event = only_completion(&capture, expected.label);
    assert_event_matches(&event, expected);
    let latency = recorded_latency(&event, expected.label);
    assert!(
        latency >= holding.as_millis(),
        "{}: the record covers only the head, not the held body: {latency}ms of at least {}ms",
        expected.label,
        holding.as_millis(),
    );
    assert!(
        latency <= opened.elapsed().as_millis(),
        "{}: the recorded span outlives the exchange the row drove",
        expected.label,
    );
}

/// 15.T1: a peer that leaves mid-body is recorded under its own terminal.
fn assert_disconnect_records_the_peer_that_left(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "disconnected",
        method: "GET",
        status: 200,
        protocol: "streaming_http",
        terminal: "disconnect",
        boundary: "none",
    };
    let before = Recorded::scraped(fixture.addr());
    let peer = peer_reading_head(fixture.addr(), STREAM_PATH, expected.label);
    drop(peer);

    let settled = http_support::poll_until(COMPLETION_BOUND, || {
        moved(&before, &Recorded::scraped(fixture.addr()), &expected) >= 1
    });
    assert!(
        settled,
        "disconnected: the peer that left was never recorded",
    );
    assert_recorded_once(&before, &Recorded::scraped(fixture.addr()), &expected);
}

/// 15.T1: a refusal is recorded under the answer the peer was given.
fn assert_refusal_records_its_answer(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "refused",
        method: ABSENT_METHOD,
        status: 404,
        protocol: "unclassified",
        terminal: "completed",
        boundary: "none",
    };
    let before = Recorded::scraped(fixture.addr());
    let refused = exchange(fixture.addr(), ABSENT_METHOD, ABSENT_PATH, b"");
    assert_eq!(refused.status, 404, "refused: wire status");
    assert_recorded_once(&before, &Recorded::scraped(fixture.addr()), &expected);
}

/// 15.T1: a payload past the route maximum names the bound it crossed.
fn assert_body_limit_records_its_boundary(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "limited",
        method: LIMITED_METHOD,
        status: 413,
        protocol: "ordinary_http",
        terminal: "route_body_limit",
        boundary: "request_body",
    };
    let capture = common::capture_events(&format!("path={LIMITED_PATH}"));
    let before = Recorded::scraped(fixture.addr());
    let refused = exchange(
        fixture.addr(),
        LIMITED_METHOD,
        LIMITED_PATH,
        &vec![b'x'; COMPLETION_MAX_BODY * 4],
    );
    assert_eq!(refused.status, 413, "limited: wire status");
    assert_recorded_once(&before, &Recorded::scraped(fixture.addr()), &expected);
    assert_event_matches(&only_completion(&capture, "limited"), &expected);
}

/// 15.T1: an operation that outlives its lifetime names that lifetime.
fn assert_request_total_records_its_boundary(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "cancelled",
        method: UNANSWERED_METHOD,
        status: 408,
        protocol: "ordinary_http",
        terminal: "request_total",
        boundary: "request_total",
    };
    let capture = common::capture_events(&format!("path={UNANSWERED_PATH}"));
    let before = Recorded::scraped(fixture.addr());
    let refused = exchange(fixture.addr(), UNANSWERED_METHOD, UNANSWERED_PATH, b"");
    assert_eq!(refused.status, 408, "cancelled: wire status");
    assert_recorded_once(&before, &Recorded::scraped(fixture.addr()), &expected);
    assert_event_matches(&only_completion(&capture, "cancelled"), &expected);
}

/// 15.T1: tonic's answer is recorded once, at the body Camber wrapped it in.
#[cfg(feature = "grpc")]
fn assert_grpc_records_at_its_terminal(fixture: &CompletionFixture) {
    let expected = Expected {
        label: "grpc",
        method: "POST",
        status: 200,
        protocol: "grpc",
        terminal: "completed",
        boundary: "none",
    };
    let before = Recorded::scraped(fixture.addr());
    let answered = common::block_on(completion_rpc(fixture.addr()));
    assert_eq!(answered.status, 200, "grpc: wire status");
    assert_recorded_once(&before, &Recorded::scraped(fixture.addr()), &expected);
}

#[cfg(not(feature = "grpc"))]
fn assert_grpc_records_at_its_terminal(_fixture: &CompletionFixture) {}

/// One unary call, framed on the wire this row's peer owns.
#[cfg(feature = "grpc")]
async fn completion_rpc(addr: std::net::SocketAddr) -> http_support::HttpResponse {
    let mut client = common::PersistentH2Client::connect(addr, COMPLETION_BOUND).await;
    let answered = client
        .send_complete(
            "POST",
            "/greeter.Greeter/SayHello",
            "localhost",
            &[("content-type", "application/grpc"), ("te", "trailers")],
            &completion_grpc_message("completion"),
        )
        .await;
    client.close().await;
    answered
}

/// One length-prefixed gRPC message carrying a `HelloRequest` named `name`.
#[cfg(feature = "grpc")]
fn completion_grpc_message(name: &str) -> Box<[u8]> {
    let mut message = Vec::with_capacity(name.len() + 2);
    message.push(0x0A);
    message.push(u8::try_from(name.len()).expect("the fixture name is short"));
    message.extend_from_slice(name.as_bytes());
    let mut framed = Vec::with_capacity(message.len() + 5);
    framed.push(0);
    framed.extend_from_slice(
        &u32::try_from(message.len())
            .expect("one short message")
            .to_be_bytes(),
    );
    framed.extend_from_slice(&message);
    framed.into_boxed_slice()
}

/// 15.T1
///
/// One completion owner follows each admitted operation to its true terminal.
/// A buffered answer, a streaming body, an SSE feed, a proxied download, and a
/// gRPC answer are each recorded once — never at the head — under the status the
/// peer was given, the whole head-to-terminal span, and the highest-precedence
/// typed boundary any owner fixed. A refusal, a peer that leaves, a payload past
/// its maximum, and an operation that outlives its lifetime are recorded the
/// same way, under the terminals that are theirs.
#[test]
fn completion_metrics_and_events_emit_once_at_true_terminal() {
    common::test_runtime()
        .with_metrics()
        .with_tracing()
        .shutdown_timeout(SHUTDOWN_BOUND)
        .run(|| {
            let fixture = CompletionFixture::serve();
            assert_buffered_records_once(&fixture);
            assert_held_class_records_at_its_body(
                &fixture,
                STREAM_PATH,
                &fixture.stream,
                &Expected {
                    label: "streaming",
                    method: "GET",
                    status: 200,
                    protocol: "streaming_http",
                    terminal: "completed",
                    boundary: "none",
                },
            );
            assert_held_class_records_at_its_body(
                &fixture,
                SSE_PATH,
                &fixture.sse,
                &Expected {
                    label: "sse",
                    method: "GET",
                    status: 200,
                    protocol: "server_sent_events",
                    terminal: "completed",
                    boundary: "none",
                },
            );
            assert_held_class_records_at_its_body(
                &fixture,
                PROXY_PATH,
                &fixture.upstream_stream,
                &Expected {
                    label: "streaming proxy",
                    method: "GET",
                    status: 200,
                    protocol: "proxy",
                    terminal: "completed",
                    boundary: "none",
                },
            );
            assert_grpc_records_at_its_terminal(&fixture);
            assert_upgrade_records_at_its_handoff(&fixture);
            assert_refusal_records_its_answer(&fixture);
            assert_disconnect_records_the_peer_that_left(&fixture);
            assert_body_limit_records_its_boundary(&fixture);
            assert_request_total_records_its_boundary(&fixture);
            fixture.tear_down("completion");
            runtime::request_shutdown();
        })
        .expect("the completion fixture runtime ran to completion");
}

// ---------------------------------------------------------------------------
// 15.T2: what a completion label is allowed to carry
// ---------------------------------------------------------------------------

/// The label names every completion sample carries, declared in sorted order.
///
/// The order is part of the declaration: the check below sorts only the names it
/// scraped and compares them against this as it stands.
const COMPLETION_LABELS: [&str; 5] = ["boundary", "method", "protocol", "status", "terminal"];

/// A request identifier a peer supplies, which Camber must not adopt.
const SUPPLIED_REQUEST_ID: &str = "peer-supplied-identity-0123456789";

/// A diagnostic the fixture's own mapper states for the operator alone.
const MAPPER_DETAIL: &str = "completion-mapper-detail";

/// One journey the label row drives, and what that journey varies.
struct Varied {
    label: &'static str,
    method: &'static str,
    path: &'static str,
    /// How many payload bytes the peer sends.
    payload: usize,
    status: u16,
}

impl Varied {
    /// Whether Camber's own answer to this row names the request.
    ///
    /// A refusal is answered through the fixture's mapper, which names the
    /// request the way the built-in answer would; a handler's own `200` is the
    /// application's response and carries nothing Camber added.
    const fn names_its_identity(&self) -> bool {
        self.status != 200
    }
}

/// Every journey the label row drives.
///
/// Between them they vary a peer-chosen identifier, four raw paths, a crossed
/// bound, an unreachable upstream, and the detail the fixture's own mapper
/// states. Each of those is something production is holding when it writes this
/// row's labels, which is what makes the exclusion below a claim about this
/// service rather than a search for a string nothing ever had.
const VARIED_ROWS: [Varied; 4] = [
    Varied {
        label: "varied buffered",
        method: BUFFERED_METHOD,
        path: BUFFERED_PATH,
        payload: 2,
        status: 200,
    },
    Varied {
        label: "varied absent",
        method: ABSENT_METHOD,
        path: ABSENT_PATH,
        payload: 0,
        status: 404,
    },
    Varied {
        label: "varied limited",
        method: LIMITED_METHOD,
        path: LIMITED_PATH,
        payload: COMPLETION_MAX_BODY * 4,
        status: 413,
    },
    Varied {
        label: "varied dead upstream",
        method: DEAD_METHOD,
        path: DEAD_PATH,
        payload: 0,
        status: 502,
    },
];

/// Assert one sample is labelled with exactly the closed set of names.
fn assert_closed_label_names(sample: &common::Sample) {
    let labels = sample.labels();
    assert_eq!(
        labels.len(),
        COMPLETION_LABELS.len(),
        "the completion sample {sample:?} carries {} labels rather than the closed set \
         {COMPLETION_LABELS:?}",
        labels.len(),
    );
    let mut names = [""; COMPLETION_LABELS.len()];
    for (slot, (name, _)) in names.iter_mut().zip(labels) {
        *slot = name.as_ref();
    }
    names.sort_unstable();
    assert_eq!(
        names, COMPLETION_LABELS,
        "the completion sample {sample:?} carries labels outside the closed set",
    );
}

/// Assert one label names a value its own published vocabulary declares.
fn assert_closed_value(sample: &common::Sample, name: &str, vocabulary: &[&str]) {
    assert!(
        !vocabulary.is_empty(),
        "the {name} vocabulary is empty, so no sample can be checked against it",
    );
    let value = sample
        .label(name)
        .unwrap_or_else(|| panic!("the completion sample {sample:?} carries no {name}"));
    assert!(
        vocabulary.contains(&value),
        "the completion sample {sample:?} names the {name} {value:?}, which production \
         does not declare: {vocabulary:?}",
    );
}

/// Assert no label on one sample carries anything one request can vary.
fn assert_bounded_values(sample: &common::Sample, forbidden: &[&str]) {
    assert!(
        !forbidden.is_empty(),
        "no varying values were declared, so no completion label can be checked against them",
    );
    for (_, value) in sample.labels() {
        assert!(
            !forbidden.iter().any(|varying| value.contains(varying)),
            "the completion sample {sample:?} carries the varying value {value:?}",
        );
    }
}

/// Everything a completion label must never carry, taken from one drive.
///
/// `recorded` is what this drive read back out of production's own records —
/// the identifier it minted for each row and the private cause it stated for
/// each refusal — and `held` is what production handed its own mapper, the safe
/// message and the peer address it reported. None of that is written down here:
/// a value the test chose is a value production may never have had, and a label
/// search for one of those cannot fail.
fn varying_values<'a>(recorded: &'a [Box<str>], held: &'a [Box<str>]) -> Box<[&'a str]> {
    assert!(
        !recorded.is_empty() && !held.is_empty(),
        "the drive produced no recorded identity or nothing production held, so the \
         exclusion below would search for nothing this service had: {recorded:?} / {held:?}",
    );
    [SUPPLIED_REQUEST_ID, MAPPER_DETAIL]
        .into_iter()
        .chain(recorded.iter().map(Box::as_ref))
        .chain(held.iter().map(Box::as_ref))
        .chain(VARIED_ROWS.iter().map(|row| row.path))
        .chain([HELD_UPSTREAM_PATH, STREAM_PATH, SSE_PATH, PROXY_PATH])
        .collect()
}

/// The one completion event this row's raw path produced.
///
/// Bounded rather than read once, because the record is written where the
/// operation ends: for a refused row that is after the peer already has its
/// answer, so a straight read could ask before production wrote anything.
fn completion_event_of(capture: &common::TraceCapture, row: &Varied) -> Box<str> {
    let settled = http_support::poll_until(COMPLETION_BOUND, || !capture.is_empty());
    assert!(
        settled,
        "{}: no completion event was recorded for {}",
        row.label, row.path,
    );
    only_completion(capture, row.label)
}

/// The private cause one refused row recorded for operators alone.
///
/// Read off production's own rejection event rather than out of the mapper: a
/// configured mapper is deliberately never handed the diagnostic, so this event
/// is the only place the service states what actually went wrong. The cause is
/// the last field the event carries, so the whole tail after it is that text —
/// and it is text no completion label may repeat.
fn recorded_cause(capture: &common::TraceCapture, row: &Varied) -> Box<str> {
    let events = capture.events();
    let event = common::only_event(&events, REJECTION_EVENT, row.label);
    let at = event.find(CAUSE_FIELD).unwrap_or_else(|| {
        panic!(
            "{}: the rejection event states no cause: {event}",
            row.label
        )
    });
    let cause = &event[at + CAUSE_FIELD.len()..];
    assert!(
        !cause.is_empty(),
        "{}: the rejection event states an empty cause: {event}",
        row.label,
    );
    cause.into()
}

/// Assert one row's variable data reached the event, and name the identity.
///
/// The positive half of what the labels prove the negative of. The identifier
/// Camber minted and the raw path the peer asked for are both recorded, in the
/// structured event, which is the one place they are allowed — and the
/// identifier is Camber's own rather than the one the peer supplied.
///
/// Returns the identity production recorded, so the label check below searches
/// for the exact value the recorder held.
fn assert_identity_reaches_the_event(
    event: &str,
    row: &Varied,
    answered: &http_support::HttpResponse,
) -> Box<str> {
    common::assert_field_value(event, "path", row.path, row.label);
    common::assert_field_value(event, "status", &row.status.to_string(), row.label);
    let recorded = common::field_value(event, "request_id").unwrap_or_else(|| {
        panic!(
            "{}: the completion event names no request identifier: {event}",
            row.label,
        )
    });
    common::assert_request_id_shape(Some(recorded), row.label);
    assert_ne!(
        recorded, SUPPLIED_REQUEST_ID,
        "{}: Camber adopted the identifier its peer supplied",
        row.label,
    );
    match row.names_its_identity() {
        true => assert_eq!(
            recorded,
            common::request_id_of(answered, row.label).as_ref(),
            "{}: the event names a different request than the answer did",
            row.label,
        ),
        false => {}
    }
    recorded.into()
}

/// Assert every scraped completion sample carries only closed vocabularies.
fn assert_bounded_completion_labels(samples: &[common::Sample], forbidden: &[&str]) {
    assert!(!samples.is_empty(), "no completion samples were scraped");
    let vocabulary = camber::http::mock::completion_vocabulary();
    for sample in samples {
        assert_closed_label_names(sample);
        assert_closed_value(sample, "method", &vocabulary.methods);
        assert_closed_value(sample, "protocol", &vocabulary.protocols);
        assert_closed_value(sample, "terminal", &vocabulary.terminals);
        assert_closed_value(sample, "boundary", &vocabulary.boundaries);
        assert_bounded_values(sample, forbidden);
    }
}

/// Assert the required closed labels are actually present and populated.
///
/// The vocabulary checks above hold vacuously over a scrape carrying neither
/// label, which is exactly the tree this step started from. This is the half
/// that fails there: the terminal and the boundary must exist, and between them
/// the drive below must have produced a named bound rather than only absences.
fn assert_required_labels_are_populated(samples: &[common::Sample]) {
    let named = samples
        .iter()
        .filter(|sample| {
            sample
                .label("boundary")
                .is_some_and(|value| value != "none")
        })
        .count();
    assert!(
        named > 0,
        "no completion sample names a crossed bound, so the boundary label proves nothing: \
         {samples:?}",
    );
    let terminals = samples
        .iter()
        .filter_map(|sample| sample.label("terminal"))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        terminals.len() > 1,
        "every completion sample reports the same terminal, so the terminal label \
         distinguishes nothing: {terminals:?}",
    );
}

/// Drive every varied journey and report the identities production recorded.
///
/// Each row is captured on its own raw path, so the event read back is that
/// row's own record rather than the first of several. The crossed bound and the
/// unreachable upstream are rows here rather than extras beside the loop: the
/// boundary label needs a populated value, and the mapper needs a real upstream
/// diagnostic to state its detail over.
fn drive_varied_rows(addr: std::net::SocketAddr) -> Box<[Box<str>]> {
    let mut recorded = Vec::new();
    for row in &VARIED_ROWS {
        // One needle for both records: a rejection event names this row's raw
        // path in `raw_path`, which carries the completion event's own `path`
        // field inside it, so the two arrive in one capture and are told apart
        // by the sentence each states.
        let capture = common::capture_events(&format!("path={}", row.path));
        let answered = http_support::send(
            addr,
            row.method,
            row.path,
            &[("X-Request-Id", SUPPLIED_REQUEST_ID)],
            &vec![b'x'; row.payload],
        );
        assert_eq!(
            answered.status, row.status,
            "{}: unexpected wire status",
            row.label,
        );
        let event = completion_event_of(&capture, row);
        recorded.push(assert_identity_reaches_the_event(&event, row, &answered));
        match row.names_its_identity() {
            // The mapper's detail is stated on the wire, so the value the
            // labels are then searched for is one this service demonstrably
            // held rather than one this row wrote down and never gave away.
            true => {
                assert!(
                    String::from_utf8_lossy(&answered.body).contains(MAPPER_DETAIL),
                    "{}: the fixture's mapper never stated its detail: {:?}",
                    row.label,
                    String::from_utf8_lossy(&answered.body),
                );
                recorded.push(recorded_cause(&capture, row));
            }
            false => {}
        }
    }
    recorded.into_boxed_slice()
}

/// 15.T2
///
/// Varied request identifiers, raw paths, peer addresses, upstream diagnostics,
/// and mapper detail reach a live service. Every one of them is recorded in the
/// structured event, which is where variable data is allowed. What the
/// completion counters carry is only the closed method, status, dispatch-class,
/// terminal, and boundary vocabularies production itself publishes, and no
/// value this drive proved production was holding reaches a label.
#[test]
fn completion_labels_exclude_unbounded_identity_and_diagnostics() {
    common::test_runtime()
        .with_metrics()
        .with_tracing()
        .shutdown_timeout(SHUTDOWN_BOUND)
        .run(|| {
            let fixture = CompletionFixture::serve();
            let addr = fixture.addr();

            let recorded = drive_varied_rows(addr);
            // Taken after the drive: these are the diagnostic and the peer
            // address production handed its own mapper, so the exclusion below
            // searches for what the recorder was holding rather than for text
            // this row wrote down.
            let held = fixture.mapper.taken();
            let forbidden = varying_values(&recorded, &held);

            let scraped = Recorded::scraped(addr);
            assert_bounded_completion_labels(&scraped.completions, &forbidden);
            assert_bounded_completion_labels(&scraped.durations, &forbidden);
            assert_required_labels_are_populated(&scraped.completions);

            fixture.tear_down("completion labels");
            runtime::request_shutdown();
        })
        .expect("the completion label fixture runtime ran to completion");
}

// ---------------------------------------------------------------------------
// 17.T4
// ---------------------------------------------------------------------------

/// The fixed sentence every failed resource callback is reported under.
const RESOURCE_FAILURE_EVENT: &str = "resource callback failed";

/// The resources one periodic pass publishes, in registration order.
///
/// Deliberately not in alphabetical order. A projection that sorted its keys
/// would answer with a different sequence, so the order alone tells the
/// coordinator's own visit order from a serializer's.
const PUBLISHED_RESOURCES: [&str; 6] = [
    "zulu-well",
    "alpha-returned",
    "mike-panicked",
    "bravo-deadline",
    "yankee-lost",
    "charlie-blocked",
];

/// What `/health` says once every resource has reached its scripted state.
const PUBLISHED_HEALTH: &str = concat!(
    r#"{"status":"unhealthy","resources":{"#,
    r#""zulu-well":{"status":"ok"},"#,
    r#""alpha-returned":{"status":"error","failure":"returned"},"#,
    r#""mike-panicked":{"status":"error","failure":"panicked"},"#,
    r#""bravo-deadline":{"status":"error","failure":"deadline"},"#,
    r#""yankee-lost":{"status":"error","failure":"lost-worker"},"#,
    r#""charlie-blocked":{"status":"error","failure":"blocked"}}}"#,
);

#[test]
fn periodic_resource_failures_reach_live_health_and_events() {
    let capture = common::capture_events(RESOURCE_FAILURE_EVENT);
    let controller = camber::runtime_test_support::runtime_schedule();
    let log = std::sync::Arc::new(common::CallbackLog::default());

    let blocked = common::ScriptedResource::new("charlie-blocked", &log)
        .health(common::Behavior::ParkFrom(2));
    let deadline =
        common::ScriptedResource::new("bravo-deadline", &log).health(common::Behavior::ParkFrom(3));
    let gates = [blocked.gate(), deadline.gate()];
    let witnesses = [blocked.drop_witness(), deadline.drop_witness()];

    let published = publish_resource_states(&controller, &log, blocked, deadline, gates.clone());

    assert_eq!(
        &*published.health, PUBLISHED_HEALTH,
        "the live health projection did not name every resource's closed outcome in order"
    );
    assert_eq!(
        published.status, 503,
        "a projection carrying five failures answered as if the service were well"
    );
    assert_causes_stay_in_operator_fields(&capture, &published);
    assert_no_resource_names_in_metrics(&published.metrics);

    for gate in &gates {
        gate.open();
    }
    common::wait_until("every released worker to let its resource go", || {
        witnesses
            .iter()
            .all(|w| w.load(std::sync::atomic::Ordering::Acquire))
    });
}

/// What one live read of a running service's operator surfaces returned.
struct PublishedState {
    status: u16,
    health: Box<str>,
    metrics: Box<str>,
}

/// Serve the six scripted resources and read the operator surfaces once every
/// one of them has reached its scripted state.
fn publish_resource_states(
    controller: &camber::runtime_test_support::RuntimeController,
    log: &std::sync::Arc<common::CallbackLog>,
    blocked: common::ScriptedResource,
    deadline: common::ScriptedResource,
    gates: [std::sync::Arc<common::Gate>; 2],
) -> PublishedState {
    runtime::builder()
        .with_test_schedule(controller)
        .with_metrics()
        .with_tracing()
        .shutdown_timeout(SHUTDOWN_BOUND)
        .health_interval(common::TICK)
        .resource_budget(common::short_resource_budget())
        .resource(common::ScriptedResource::new("zulu-well", log))
        .resource(
            common::ScriptedResource::new("alpha-returned", log)
                .health(common::Behavior::FailFrom(2)),
        )
        .resource(
            common::ScriptedResource::new("mike-panicked", log)
                .health(common::Behavior::PanicFrom(2)),
        )
        .resource(deadline)
        .resource(common::ScriptedResource::new("yankee-lost", log))
        .resource(blocked)
        .run(|| {
            // Refused only now: the readiness pass has already run, and a
            // refusal standing through it would have stopped the service before
            // any periodic state existed to publish.
            controller.refuse_resource_worker("yankee-lost");
            let addr = common::spawn_server(camber::http::Router::new());
            let published = read_until_published(addr);
            // Released before teardown, so this row's claim about a live
            // projection is not also a claim about what a still-parked worker
            // does to the returned aggregate — 17.T5 owns that.
            for gate in &gates {
                gate.open();
            }
            controller.admit_resource_worker("yankee-lost");
            runtime::request_shutdown();
            published
        })
        .expect("the published resource fixture runtime ran to completion")
}

/// Read `/health` until every resource has published its scripted state, then
/// read the operator surfaces the row asserts on.
///
/// Polled rather than slept on: the six states settle across two periodic
/// passes, and the last of them is a resource whose callback never runs, so
/// there is nothing else for the row to wait on.
fn read_until_published(addr: std::net::SocketAddr) -> PublishedState {
    let mut health = health_read(addr);
    let deadline = std::time::Instant::now() + common::OBSERVATION_BOUND;
    while health.1.as_ref() != PUBLISHED_HEALTH && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        health = health_read(addr);
    }
    let metrics = body_text(&http_support::send(addr, "GET", "/metrics", &[], b""));
    PublishedState {
        status: health.0,
        health: health.1,
        metrics,
    }
}

/// One live read of the health endpoint: the status it answered and its body.
fn health_read(addr: std::net::SocketAddr) -> (u16, Box<str>) {
    let response = http_support::send(addr, "GET", "/health", &[], b"");
    (response.status, body_text(&response))
}

/// One answer's body, as the text an operator reads.
fn body_text(response: &http_support::HttpResponse) -> Box<str> {
    String::from_utf8_lossy(&response.body)
        .into_owned()
        .into_boxed_str()
}

/// Every arbitrary cause reaches an operator event, and none of them reaches
/// the body a peer reads.
fn assert_causes_stay_in_operator_fields(
    capture: &common::TraceCapture,
    published: &PublishedState,
) {
    let events = capture.events();
    for (resource, failure, cause) in [
        ("alpha-returned", "returned", common::SCRIPTED_REFUSAL),
        ("mike-panicked", "panicked", common::SCRIPTED_PANIC),
    ] {
        let reported = events
            .iter()
            .find(|event| event.contains(&format!("resource={resource}")))
            .unwrap_or_else(|| panic!("no operator event named {resource}: {events:?}"));
        common::assert_field_value(reported, "failure", failure, resource);
        assert!(
            reported.contains(cause),
            "{resource}: the event dropped the callback's own cause: {reported}"
        );
        assert!(
            !published.health.contains(cause),
            "{resource}: an arbitrary cause reached the health body: {}",
            published.health
        );
    }
}

/// No resource name became a metric label.
fn assert_no_resource_names_in_metrics(metrics: &str) {
    for resource in PUBLISHED_RESOURCES {
        assert!(
            !metrics.contains(resource),
            "{resource} appeared in the metrics exposition: {metrics}"
        );
    }
}
