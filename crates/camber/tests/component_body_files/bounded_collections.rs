//! 9.T2 and 9.T3: static files are read under one checked ceiling, off every
//! Tokio worker, and never retain the chunk that crosses that ceiling.

use crate::http as http_support;
use crate::rejection_support;
use crate::runtime_support as common;
use crate::trace_capture;

use camber::__private::DEFAULT_STATIC_FILE_LIMIT;
use camber::RuntimeError;
use camber::http::mock::{self, BlockingWorkerEdge};
use camber::http::{ByteBoundary, Router};
use camber::runtime;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// The ceiling every measured row is read under.
const CEILING: usize = 16;

/// The payload that exactly fills [`CEILING`].
const ADMITTED: &str = "0123456789abcdef";

/// What one row's file holds once it has outgrown [`CEILING`].
///
/// Exactly twice the ceiling, so a reader that measured the file at the ceiling
/// takes one admitted chunk and one that crosses. A row asserting the crossing
/// chunk was dropped needs both to exist.
const GROWN: &str = "0123456789abcdef0123456789abcdef";

/// How long a released worker has to hand back what it owns.
const WORKER_BOUND: Duration = Duration::from_secs(5);

/// The maximum an explicit opt-out reports having frozen.
///
/// Production compares every collection against one number, and the opt-out's
/// number is the largest total a collection could ever reach. Naming it here is
/// what lets a row say "this read froze no ceiling" as a value rather than as
/// the absence of one.
const UNBOUNDED: usize = usize::MAX;

/// The row label every assertion about the routed crossing reports under.
const ROUTED_CROSSING: &str = "the routed crossing";

/// The typed cause an operator reads when a static-file maximum is crossed.
const STATIC_FILE_CEILING_CAUSE: &str = "cause=byte limit exceeded: static_file";

/// The category a routed static-file crossing is recorded under.
///
/// A served root holding content the service cannot answer with is the
/// operator's configuration, so the refusal is Camber's fault and not the
/// peer's request.
const ROUTED_CROSSING_KIND: &str = "kind=internal_service";

/// One temporary root, the observer bound to it, and the files under it.
///
/// The observer is keyed by the root a caller names, so each row watches only
/// its own reads: two rows sharing one registration would report each other's
/// chunks as their own. The controller is declared first so it closes before
/// the directory it watches is removed.
struct StaticRoot {
    controller: mock::ScopedBlockingWorker,
    dir: tempfile::TempDir,
}

impl StaticRoot {
    /// A watched root holding one file.
    fn holding(name: &str, contents: &str) -> Self {
        let dir = tempfile::tempdir().expect("a temporary static root");
        let root = Self {
            controller: mock::static_file_worker(dir.path()).expect("one observer per root"),
            dir,
        };
        root.write(name, contents);
        root
    }

    /// Replace one file's whole contents, on disk, before this returns.
    fn write(&self, name: &str, contents: &str) {
        let mut file = std::fs::File::create(self.dir.path().join(name)).expect("a static file");
        file.write_all(contents.as_bytes())
            .expect("the file's contents");
        file.sync_all().expect("the file on disk");
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn as_str(&self) -> &str {
        self.path().to_str().expect("a utf-8 temporary root")
    }
}

/// What one bounded read published while it answered.
struct Retention {
    /// How many chunks the collector was handed.
    polled: usize,
    /// The most this read ever held at once.
    peak: usize,
}

impl Retention {
    fn of(controller: &mock::WorkerRetention) -> Self {
        Self {
            polled: controller.collected.collected_chunks_polled(),
            peak: controller.collected.collected_peak_retained_bytes(),
        }
    }
}

/// The maximum one root's most recent read was actually collected under.
///
/// Published by the collector that enforces it, so a spelling that froze no
/// ceiling reports [`UNBOUNDED`] here however small the file it served was.
fn frozen_ceiling(controller: &mock::WorkerRetention) -> usize {
    controller.worker.static_files_observed().frozen_ceiling
}

/// Assert one answer is the typed static-file ceiling refusal.
fn assert_ceiling_crossed(refusal: &RuntimeError, label: &str) {
    assert!(
        matches!(
            refusal,
            RuntimeError::LimitExceeded(ByteBoundary::StaticFile)
        ),
        "expected {label} to name the static-file maximum, got {refusal:?}",
    );
}

/// A file that exactly fills its ceiling is served whole.
async fn assert_exact_limit_is_served() {
    let root = StaticRoot::holding("exact.txt", ADMITTED);
    let served = camber::http::serve_file_with_limit(root.path(), "exact.txt", CEILING)
        .await
        .expect("a file that exactly fills its ceiling");

    assert_eq!(served.status(), 200);
    assert_eq!(served.body(), ADMITTED);
    assert_eq!(Retention::of(&root.controller).peak, ADMITTED.len());
}

/// A file that states more than its ceiling is refused before it is opened.
async fn assert_declared_oversize_is_refused_unread() {
    let root = StaticRoot::holding("oversize.txt", GROWN);
    let refusal = camber::http::serve_file_with_limit(root.path(), "oversize.txt", CEILING)
        .await
        .expect_err("a file that states more than its ceiling");

    assert_ceiling_crossed(&refusal, "a declared oversize file");
    let kept = Retention::of(&root.controller);
    assert_eq!(kept.polled, 0, "a refused declaration reads no chunk");
    assert_eq!(kept.peak, 0, "a refused declaration retains nothing");
}

/// A file that grew after it was measured is refused on the chunk that crosses.
async fn assert_growth_past_the_ceiling_drops_the_crossing_chunk() {
    let root = StaticRoot::holding("growing.txt", ADMITTED);
    root.controller
        .worker
        .pause_once(BlockingWorkerEdge::StaticFileMetadataObserved)
        .expect("one armed metadata checkpoint");

    let base = root.path().to_path_buf();
    let reading = tokio::spawn(async move {
        camber::http::serve_file_with_limit(&base, "growing.txt", CEILING).await
    });
    root.controller
        .worker
        .wait_until_paused(BlockingWorkerEdge::StaticFileMetadataObserved)
        .await
        .expect("the worker measured the file");
    root.write("growing.txt", GROWN);
    root.controller
        .worker
        .release(BlockingWorkerEdge::StaticFileMetadataObserved)
        .expect("the measured worker resumes");

    let refusal = reading
        .await
        .expect("the read task")
        .expect_err("a file that outgrew its ceiling");
    assert_ceiling_crossed(&refusal, "a file that grew past its ceiling");

    let kept = Retention::of(&root.controller);
    assert_eq!(
        kept.polled, 2,
        "the admitted chunk and the one that crossed"
    );
    assert_eq!(kept.peak, CEILING, "the crossing chunk is never retained");
}

/// Only the explicitly named opt-out reads past a ceiling.
async fn assert_only_the_unbounded_name_reads_past_the_ceiling() {
    let root = StaticRoot::holding("opted.txt", GROWN);
    let refusal = camber::http::serve_file_with_limit(root.path(), "opted.txt", CEILING)
        .await
        .expect_err("the same file under a finite ceiling");
    assert_ceiling_crossed(&refusal, "a finite ceiling on the opted-out file");

    let served = camber::http::serve_file_unbounded(root.path(), "opted.txt")
        .await
        .expect("the explicit opt-out serves what it finds");
    assert_eq!(served.status(), 200);
    assert_eq!(served.body(), GROWN);
}

/// Neither unnamed spelling is a spelling of unbounded.
///
/// The documented default is eight MiB, which no row here writes a file past,
/// so what a defaulted read answers with is exactly what an unbounded one
/// answers with. The two are told apart by the maximum production froze: a
/// defaulted read froze the documented one, and only the named opt-out froze
/// none.
async fn assert_the_default_spellings_freeze_the_documented_maximum() {
    let root = StaticRoot::holding("asset.txt", ADMITTED);

    let defaulted = camber::http::serve_file(root.path(), "asset.txt")
        .await
        .expect("the unnamed direct spelling serves a small file");
    assert_eq!(defaulted.status(), 200);
    assert_eq!(defaulted.body(), ADMITTED);
    assert_eq!(
        frozen_ceiling(&root.controller),
        DEFAULT_STATIC_FILE_LIMIT,
        "the unnamed direct spelling reads under the documented maximum",
    );

    let opted_out = camber::http::serve_file_unbounded(root.path(), "asset.txt")
        .await
        .expect("the named direct opt-out serves the same file");
    assert_eq!(opted_out.status(), 200);
    assert_eq!(
        frozen_ceiling(&root.controller),
        UNBOUNDED,
        "only the named direct opt-out reads under no maximum at all",
    );
}

/// The peer learned nothing; the operator learned which maximum was crossed.
///
/// The refusal a routed crossing becomes is redacted, so the typed provenance
/// is not on the wire to read: the operator's record is the only place the
/// promise that a routed crossing carries `ByteBoundary::StaticFile` can be
/// held to.
fn assert_the_crossing_reached_the_operator(recorded: &trace_capture::TraceCapture) {
    let events = recorded.events();
    let event = trace_capture::only_event(
        &events,
        rejection_support::REJECTION_MESSAGE,
        ROUTED_CROSSING,
    );
    trace_capture::assert_fields(
        event,
        &[STATIC_FILE_CEILING_CAUSE, ROUTED_CROSSING_KIND],
        ROUTED_CROSSING,
    );
}

/// The registered ceiling is the one a routed request is answered under.
async fn assert_routed_rows_apply_the_registered_ceiling() {
    let root = StaticRoot::holding("asset.txt", GROWN);
    let mut router = Router::new();
    router
        .static_files_with_limit("/bounded", root.as_str(), CEILING)
        .expect("a finite routed ceiling");
    router.static_files_unbounded("/unbounded", root.as_str());
    router.static_files("/default", root.as_str());
    let addr = common::spawn_server(router);

    let recorded = trace_capture::capture_events("raw_path=/bounded/asset.txt");
    let refused = camber::http::get(&format!("http://{addr}/bounded/asset.txt"))
        .await
        .unwrap();
    assert_eq!(refused.status(), 500);
    assert!(
        !refused.body().contains(GROWN),
        "no crossing byte reaches the peer: {}",
        refused.body(),
    );
    assert_eq!(
        frozen_ceiling(&root.controller),
        CEILING,
        "the finite route reads under the maximum it registered",
    );
    assert_the_crossing_reached_the_operator(&recorded);

    let opted_out = camber::http::get(&format!("http://{addr}/unbounded/asset.txt"))
        .await
        .unwrap();
    assert_eq!(opted_out.status(), 200);
    assert_eq!(opted_out.body(), GROWN);
    assert_eq!(
        frozen_ceiling(&root.controller),
        UNBOUNDED,
        "only the named routed opt-out reads under no maximum at all",
    );

    let defaulted = camber::http::get(&format!("http://{addr}/default/asset.txt"))
        .await
        .unwrap();
    assert_eq!(defaulted.status(), 200);
    assert_eq!(defaulted.body(), GROWN);
    assert_eq!(
        frozen_ceiling(&root.controller),
        DEFAULT_STATIC_FILE_LIMIT,
        "the unnamed routed spelling reads under the documented maximum",
    );

    runtime::request_shutdown();
}

/// 9.T2
#[camber::test]
async fn static_file_applies_selected_ceiling_without_crossing_retention() {
    assert_exact_limit_is_served().await;
    assert_declared_oversize_is_refused_unread().await;
    assert_growth_past_the_ceiling_drops_the_crossing_chunk().await;
    assert_only_the_unbounded_name_reads_past_the_ceiling().await;
    assert_the_default_spellings_freeze_the_documented_maximum().await;
    assert_routed_rows_apply_the_registered_ceiling().await;
}

/// A runtime with exactly one worker thread.
///
/// One worker is what makes "off the awaiting thread" mean "off every Tokio
/// worker": production records each filesystem step against the thread that
/// awaits it, and the awaiting task here is spawned onto the only worker there
/// is, so a step that ran anywhere else ran on no worker at all.
fn single_worker_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a single-worker runtime")
}

/// Assert `reads` bounded reads each resolved, measured, and read off-worker.
fn assert_off_worker(controller: &mock::WorkerRetention, reads: usize) {
    let observed = controller.worker.static_files_observed();
    assert_eq!(observed.workers_entered, reads, "workers entered");
    assert_eq!(observed.workers_returned, reads, "workers returned");
    assert_eq!(
        observed.canonicalized_off_caller, reads,
        "path confinement ran off the awaiting worker",
    );
    assert_eq!(
        observed.metadata_off_caller, reads,
        "the preflight measurement ran off the awaiting worker",
    );
    assert_eq!(
        observed.reads_off_caller, reads,
        "the checked read ran off the awaiting worker",
    );
    assert_eq!(
        observed.steps_on_caller, 0,
        "no filesystem step ran on the awaiting worker",
    );
}

/// With no runtime to offload to, the refusal precedes any filesystem work.
fn assert_no_runtime_refusal_precedes_filesystem_work() {
    let root = StaticRoot::holding("present.txt", ADMITTED);
    let mut serving = std::pin::pin!(camber::http::serve_file(root.path(), "present.txt"));

    match serving
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(Err(RuntimeError::NoRuntime)) => {}
        other => panic!("expected an absent runtime to be refused, got {other:?}"),
    }

    let observed = root.controller.worker.static_files_observed();
    assert_eq!(observed.workers_entered, 0, "nothing was offloaded");
    assert_eq!(
        observed.canonicalized_off_caller + observed.steps_on_caller,
        0,
        "no path was resolved",
    );
}

/// Every filesystem step of a direct read runs off the one worker awaiting it.
fn assert_direct_read_runs_off_the_only_worker() {
    let root = StaticRoot::holding("present.txt", ADMITTED);
    let base = root.path().to_path_buf();

    let served = single_worker_runtime().block_on(async move {
        tokio::spawn(async move { camber::http::serve_file(&base, "present.txt").await })
            .await
            .expect("the read task")
    });

    assert_eq!(
        served.expect("a file under the default ceiling").status(),
        200,
    );
    assert_off_worker(&root.controller, 1);
}

/// A routed request's file is read off the one worker its handler runs on.
fn assert_routed_read_runs_off_the_only_worker() {
    let root = StaticRoot::holding("asset.txt", ADMITTED);
    let mut router = Router::new();
    router.static_files("/assets", root.as_str());

    let answered = common::test_runtime()
        .worker_threads(1)
        .run(|| {
            let addr = common::spawn_server(router);
            let answer = http_support::request_to_host(
                addr,
                "GET",
                "/assets/asset.txt",
                http_support::DEFAULT_HOST,
                &[],
            );
            runtime::request_shutdown();
            answer
        })
        .expect("the routed fixture runtime")
        .expect("the routed answer");

    assert_eq!(answered.status, 200);
    assert_eq!(answered.text().as_ref(), ADMITTED);
    assert_off_worker(&root.controller, 1);
}

/// A caller that stops waiting leaves the worker everything it owns.
fn assert_cancelled_wait_leaves_the_worker_its_ownership() {
    let root = StaticRoot::holding("gated.txt", ADMITTED);
    let base = root.path().to_path_buf();
    root.controller
        .worker
        .pause_once(BlockingWorkerEdge::StaticFileWorkerEntered)
        .expect("one armed worker checkpoint");

    single_worker_runtime().block_on(async {
        let reading =
            tokio::spawn(async move { camber::http::serve_file(&base, "gated.txt").await });
        root.controller
            .worker
            .wait_until_paused(BlockingWorkerEdge::StaticFileWorkerEntered)
            .await
            .expect("the worker entered");

        reading.abort();
        assert!(
            reading
                .await
                .expect_err("the cancelled read reports no answer")
                .is_cancelled(),
        );

        let held = root.controller.worker.static_files_observed();
        assert_eq!(held.workers_entered, 1);
        assert_eq!(
            held.workers_returned, 0,
            "the abandoned worker still owns its paths and its buffer",
        );
        // Nothing the worker was given had been resolved, measured, or read
        // while a caller was still waiting, so everything below happens with
        // the paths and the buffer belonging to the worker alone.
        assert_eq!(
            held.canonicalized_off_caller
                + held.metadata_off_caller
                + held.reads_off_caller
                + held.steps_on_caller,
            0,
            "the abandoned work had not started when its caller left",
        );
        assert_eq!(
            Retention::of(&root.controller).peak,
            0,
            "nothing was retained before the caller left",
        );

        root.controller
            .worker
            .release(BlockingWorkerEdge::StaticFileWorkerEntered)
            .expect("the abandoned worker resumes");
    });

    assert!(
        http_support::poll_until(WORKER_BOUND, || {
            root.controller
                .worker
                .static_files_observed()
                .workers_returned
                == 1
        }),
        "the released worker returned what it owned",
    );
    // What it finished with nobody waiting is the whole ownership claim: it
    // resolved and measured the paths it copied, and it filled the buffer it
    // holds, after the only caller that could have lent it either was gone.
    assert_off_worker(&root.controller, 1);
    assert_eq!(
        Retention::of(&root.controller).peak,
        ADMITTED.len(),
        "the abandoned worker filled its own buffer",
    );
}

/// 9.T3
#[test]
fn direct_and_routed_static_files_run_bounded_reads_off_tokio_workers() {
    assert_no_runtime_refusal_precedes_filesystem_work();
    assert_direct_read_runs_off_the_only_worker();
    assert_routed_read_runs_off_the_only_worker();
    assert_cancelled_wait_leaves_the_worker_its_ownership();
}
