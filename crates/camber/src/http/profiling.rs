//! CPU profiles, sampled off every Tokio worker and retained under one maximum.
//!
//! Answering `/debug/pprof/cpu` is blocking work with an output nobody declares
//! in advance: the sampling window is a real thread sleep, and a flamegraph grows
//! with the number of distinct stacks the sampler found. Both facts belong to the
//! operation rather than to the worker that accepted the request, so the whole
//! answer is produced inside [`spawn_blocking`](tokio::task::spawn_blocking) and
//! written through [`checked_collect`](super::checked_collect), the same owner an
//! outbound client response, a buffered proxy answer, and a static file are
//! collected by.
//!
//! The renderer is told its writes were taken even when they are refused. It
//! unwraps a write failure into a panic, so a capped writer that reported
//! `io::Error` would turn a configured maximum into an unwind and lose the typed
//! bound with it. The crossing write is dropped here instead, nothing after it is
//! retained, and the refusal travels out of [`CappedRender::finish`] as
//! [`ByteBoundary::ProfilingResponse`].

use super::Response;
use super::boundary::ByteBoundary;
use super::checked_collect::CheckedCollector;
use super::mock::{BlockingWorkerObserver, LifecycleCheckpoint, LifecycleScript, ProfilingEvent};
use crate::RuntimeError;
use bytes::Bytes;
use std::sync::Arc;

/// How many samples a second the profiler asks the operating system for.
const SAMPLE_FREQUENCY: i32 = 1000;

/// What one profiling request samples, and what it may retain of the answer.
///
/// Frozen where the route is matched, so the maximum a render is measured against
/// is the serving policy's at that instant and not one re-read later.
#[derive(Clone, Copy)]
pub(super) struct ProfilingRequest {
    seconds: u64,
    /// The maximum to retain under, or `None` for the explicit opt-out.
    limit: Option<usize>,
}

impl ProfilingRequest {
    /// Freeze one request's sampling window and output maximum.
    pub(super) const fn new(seconds: u64, limit: Option<usize>) -> Self {
        Self { seconds, limit }
    }

    /// Sample, render, and retain this profile on a thread that may block.
    ///
    /// The worker owns everything it needs. A caller that stops waiting — a
    /// cancelled request, an expired deadline, a shutdown — takes nothing away
    /// from it: it keeps its buffer until it returns, and Camber stops listening.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::NoRuntime`] before any sampling when no Tokio
    /// runtime is entered, [`RuntimeError::LimitExceeded`] naming
    /// [`ByteBoundary::ProfilingResponse`] when the render crosses the frozen
    /// maximum, [`RuntimeError::Http`] when the profiler could not be started or
    /// its report could not be built, and the mapped worker failure when the
    /// blocking thread never answered.
    pub(super) async fn answer(self) -> Result<Response, RuntimeError> {
        let executor =
            tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
        let worker = ProfilingWorker::owning(self);
        match executor.spawn_blocking(move || worker.answer()).await {
            Ok(answered) => answered,
            Err(joined) => Err(super::blocking_worker_failed(joined)),
        }
    }
}

/// One profiling answer, and everything it owns while it is produced.
///
/// Assembled on the awaiting side and moved whole to the blocking worker, so the
/// thread that awaits it lends it nothing.
struct ProfilingWorker {
    request: ProfilingRequest,
    /// The process-scoped observer, when a test registered one, and the thread
    /// awaiting this answer. Inert otherwise.
    ///
    /// The shared blocking-worker context, held by every owner that answers off
    /// a Tokio worker: it resolves its script where the render runs and states
    /// the entry-and-return order once for all of them.
    observer: BlockingWorkerObserver,
}

impl ProfilingWorker {
    /// Take everything this answer needs from the awaiting side.
    fn owning(request: ProfilingRequest) -> Self {
        Self {
            request,
            observer: BlockingWorkerObserver::awaiting(),
        }
    }

    /// Sample and render this profile, from the blocking thread.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Self::render`] answered with.
    fn answer(mut self) -> Result<Response, RuntimeError> {
        self.observer.resolve(super::mock::profiling_script());
        self.observer.spanning(
            ProfilingEvent::Entered {
                off_caller: self.observer.ran_off_caller(),
            },
            ProfilingEvent::Returned,
            LifecycleCheckpoint::ProfilingWorkerEntered,
            || self.render(),
        )
    }

    /// The rendered profile this request asked for.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] naming
    /// [`ByteBoundary::ProfilingResponse`] when the render crosses the frozen
    /// maximum, and [`RuntimeError::Http`] when the profiler could not be started
    /// or its report could not be built.
    fn render(&self) -> Result<Response, RuntimeError> {
        let guard = start_profiling()?;
        std::thread::sleep(std::time::Duration::from_secs(self.request.seconds));
        let report = guard.report().build().map_err(|error| {
            RuntimeError::Http(format!("profiler report failed: {error}").into())
        })?;

        let mut rendered = CappedRender::new(self.request.limit, self.observer.shared());
        // Read back off the render rather than from the frozen argument, so what
        // is reported is the number this answer is actually measured against.
        self.observer
            .publish(ProfilingEvent::CeilingFrozen(rendered.ceiling()));
        let written = report.flamegraph(&mut rendered);
        let svg = rendered.finish(written)?;
        Ok(Response::bytes_raw(200, svg).with_content_type("image/svg+xml"))
    }
}

/// Start the process profiler, or report why it could not start.
///
/// # Errors
///
/// Returns [`RuntimeError::Http`] when the profiler cannot be registered, which
/// is what a second concurrent profile of one process gets.
fn start_profiling() -> Result<pprof::ProfilerGuard<'static>, RuntimeError> {
    pprof::ProfilerGuardBuilder::default()
        .frequency(SAMPLE_FREQUENCY)
        .build()
        .map_err(|error| RuntimeError::Http(format!("profiler start failed: {error}").into()))
}

/// One rendered profile, accounted for before it is retained.
struct CappedRender {
    collected: CheckedCollector,
    /// Whether a write has already crossed the frozen maximum.
    crossed: bool,
}

impl CappedRender {
    /// Start a render under `limit`, reporting to `observer` when one watches.
    fn new(limit: Option<usize>, observer: Option<Arc<LifecycleScript>>) -> Self {
        Self {
            collected: CheckedCollector::new(ByteBoundary::ProfilingResponse, limit, observer),
            crossed: false,
        }
    }

    /// The maximum this render measures against.
    fn ceiling(&self) -> usize {
        self.collected.ceiling()
    }

    /// The bytes this render retained, or the refusal that ended it.
    ///
    /// A crossing outranks whatever the renderer reported afterwards: the render
    /// that stopped producing a usable document stopped because this owner
    /// refused a write, and naming the renderer's own complaint instead would
    /// hide the configured bound behind it.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] naming
    /// [`ByteBoundary::ProfilingResponse`] when a write crossed the maximum, and
    /// [`RuntimeError::Http`] when the renderer failed for its own reasons.
    fn finish(self, written: pprof::Result<()>) -> Result<Bytes, RuntimeError> {
        match (self.crossed, written) {
            (true, _) => Err(RuntimeError::LimitExceeded(ByteBoundary::ProfilingResponse)),
            (false, Ok(())) => Ok(self.collected.finish()),
            (false, Err(error)) => Err(RuntimeError::Http(
                format!("flamegraph generation failed: {error}").into(),
            )),
        }
    }
}

impl std::io::Write for CappedRender {
    /// Account for one write, then keep it — or drop it and stop retaining.
    ///
    /// The renderer is always told the bytes were taken, for the reason this
    /// module's header states: it unwraps a write failure. Once a write has
    /// crossed, nothing further reaches the collector at all, so the remainder of
    /// a refused document is neither counted nor held.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.crossed {
            true => {}
            false => self.crossed = self.collected.retain_slice(buf).is_err(),
        }
        Ok(buf.len())
    }

    /// Nothing is buffered on the way to the collector, so nothing is pending.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
