//! Static files, read under one ceiling and off every Tokio worker.
//!
//! Serving a file is blocking work with an unbounded input: the filesystem
//! answers when it answers, and a file is as large as whoever wrote it made it.
//! Both facts belong to the public operation rather than to its callers, so
//! every entry point here is `async` and fallible. It offloads the filesystem
//! to [`spawn_blocking`](tokio::task::spawn_blocking) and reads through
//! [`checked_collect`](super::checked_collect), the same owner an outbound
//! client response and a buffered proxy answer are collected by.
//!
//! The worker owns copies of the paths it was given, not borrows of the
//! caller's. A caller that stops waiting — a cancelled request, an expired
//! deadline, a shutdown — takes nothing away from it: the worker keeps its
//! paths and its buffer until it returns, and Camber simply stops listening.
//!
//! Metadata is a preflight hint and nothing more. A file that states more than
//! the ceiling is refused before it is opened, and a file that grew after it
//! was measured still crosses on the chunk that carries it past the maximum.

use super::Response;
use super::boundary::ByteBoundary;
use super::checked_collect::CheckedCollector;
use super::mock::{BlockingWorkerEdge, BlockingWorkerObserver, StaticFileEvent, StaticFileStep};
use crate::RuntimeError;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The maximum a static file is read and retained under when no caller names
/// one: eight MiB, the same ceiling Camber's ordinary buffered collections use.
pub const DEFAULT_STATIC_FILE_LIMIT: usize = 8 * 1024 * 1024;

/// The largest window one read step takes from a file at a time.
const READ_WINDOW: usize = 64 * 1024;

/// The maximum a static-file entry point freezes before it owns anything.
///
/// The one place a caller-stated maximum becomes a value the reader trusts.
/// Zero is refused here, ahead of the path copy, the runtime check, and the
/// worker — so a zero ceiling can never reach the filesystem, and zero is never
/// a spelling of unbounded.
///
/// # Errors
///
/// Returns [`RuntimeError::InvalidArgument`] naming `max_bytes` when the stated
/// maximum is zero.
pub fn frozen_static_file_limit(max_bytes: usize) -> Result<Option<usize>, RuntimeError> {
    match max_bytes {
        0 => Err(RuntimeError::InvalidArgument(
            "static file max_bytes must be greater than zero".into(),
        )),
        finite => Ok(Some(finite)),
    }
}

/// Serve a static file from `base_dir` using the relative `file_path`.
///
/// The whole file is read into memory before the response is sent, under a
/// default maximum of eight MiB. This is for small assets — HTML, CSS, JS,
/// images — not for streaming large files. Use
/// [`serve_file_with_limit`] to name a different maximum, or a dedicated file
/// server or CDN for large content.
///
/// Answers 404 for a missing path, a directory-traversal attempt, and anything
/// under the root that is not a regular file. Content-Type is chosen from the
/// file extension.
///
/// # Errors
///
/// Returns [`RuntimeError::NoRuntime`] when no Tokio runtime is entered,
/// [`RuntimeError::LimitExceeded`] naming [`ByteBoundary::StaticFile`] when the
/// file crosses the maximum, and [`RuntimeError::Io`] when a file that opened
/// cannot be read.
pub async fn serve_file(base_dir: &Path, file_path: &str) -> Result<Response, RuntimeError> {
    serve_file_with_limit(base_dir, file_path, DEFAULT_STATIC_FILE_LIMIT).await
}

/// Serve a static file under a maximum of exactly `max_bytes`.
///
/// [`serve_file`] with the ceiling named rather than defaulted. Everything else
/// — path confinement, MIME selection, 404 behavior, and off-worker reading —
/// is identical.
///
/// # Errors
///
/// Returns [`RuntimeError::InvalidArgument`] when `max_bytes` is zero, before
/// any filesystem work, and otherwise the errors [`serve_file`] returns.
pub async fn serve_file_with_limit(
    base_dir: &Path,
    file_path: &str,
    max_bytes: usize,
) -> Result<Response, RuntimeError> {
    read_bounded(base_dir, file_path, frozen_static_file_limit(max_bytes)?).await
}

/// Serve a static file with no maximum at all.
///
/// The explicit opt-out, and the only spelling that removes the ceiling. The
/// whole file is retained in memory whatever its size, so this belongs to
/// content the process itself controls — assets shipped with the service, or a
/// root written only by the operator. Pointing it at a directory anything
/// untrusted can write to makes the file's author the author of this process's
/// memory use.
///
/// # Errors
///
/// Returns the errors [`serve_file`] returns, except the ceiling refusal this
/// entry point has opted out of.
pub async fn serve_file_unbounded(
    base_dir: &Path,
    file_path: &str,
) -> Result<Response, RuntimeError> {
    read_bounded(base_dir, file_path, None).await
}

/// Read one file under `limit`, on a thread that is not a Tokio worker.
///
/// The shared owner behind every public entry point and the routed
/// [`Router::static_files`](super::Router::static_files) family, so a file one
/// of them would refuse is never admitted by another.
///
/// # Errors
///
/// Returns [`RuntimeError::NoRuntime`] before any filesystem work when no Tokio
/// runtime is entered, and otherwise whatever the worker answered with.
pub(super) async fn read_bounded(
    base_dir: &Path,
    file_path: &str,
    limit: Option<usize>,
) -> Result<Response, RuntimeError> {
    let executor = tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
    let worker = StaticFileWorker::owning(base_dir, file_path, limit);
    match executor.spawn_blocking(move || worker.answer()).await {
        Ok(answered) => answered,
        Err(joined) => Err(super::blocking_worker_failed(joined)),
    }
}

/// One static-file read, and everything it owns while it runs.
///
/// Assembled on the awaiting side and moved whole to the blocking worker. The
/// paths are copies rather than borrows, which is what lets a caller stop
/// waiting without taking anything the worker is still using.
struct StaticFileWorker {
    base_dir: Box<Path>,
    file_path: Box<str>,
    /// The maximum to retain under, or `None` for the explicit opt-out.
    limit: Option<usize>,
    /// The root-scoped observer, when a test registered one, and the thread
    /// awaiting this read. Inert otherwise.
    ///
    /// The shared blocking-worker context, held by every owner that answers off
    /// a Tokio worker: it resolves its script where the read runs and states the
    /// entry-and-return order once for all of them.
    observer: BlockingWorkerObserver,
}

impl StaticFileWorker {
    /// Take owned copies of everything this read needs.
    ///
    /// Everything except the script, which [`Self::answer`] resolves once it is
    /// running where nothing it looks up is lent back to the awaiting worker.
    fn owning(base_dir: &Path, file_path: &str, limit: Option<usize>) -> Self {
        Self {
            observer: BlockingWorkerObserver::awaiting(),
            base_dir: base_dir.into(),
            file_path: file_path.into(),
            limit,
        }
    }

    /// Resolve, measure, and read this worker's file, from the blocking thread.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] naming
    /// [`ByteBoundary::StaticFile`] when the file crosses the maximum, and
    /// [`RuntimeError::Io`] when a file that opened cannot be read.
    fn answer(mut self) -> Result<Response, RuntimeError> {
        self.observer
            .resolve(super::mock::static_file_script(&self.base_dir));
        self.observer.spanning(
            StaticFileEvent::WorkerEntered,
            StaticFileEvent::WorkerReturned,
            BlockingWorkerEdge::StaticFileWorkerEntered,
            || self.read(),
        )
    }

    /// Run one filesystem step, reporting the thread it ran on.
    fn step<T>(&self, step: StaticFileStep, work: impl FnOnce() -> T) -> T {
        self.observer.publish(StaticFileEvent::Step {
            step,
            off_caller: self.observer.ran_off_caller(),
        });
        work()
    }

    /// This worker's answer, once it is running where it may block.
    fn read(&self) -> Result<Response, RuntimeError> {
        let canonical = match self.confine() {
            Some(canonical) => canonical,
            None => return Ok(not_found()),
        };
        let declared = match self.declared(&canonical) {
            Some(declared) => declared,
            None => return Ok(not_found()),
        };

        let mut collected =
            CheckedCollector::new(ByteBoundary::StaticFile, self.limit, self.observer.shared());
        // Read back off the collector rather than from `self.limit`, so what is
        // reported is the number this read is actually measured against and not
        // the argument that was meant to become it.
        self.observer
            .publish(StaticFileEvent::CeilingFrozen(collected.ceiling()));
        collected.admit_declared(Some(declared))?;
        self.observer
            .hold_at(BlockingWorkerEdge::StaticFileMetadataObserved);

        let opened = match self.open(&canonical) {
            Some(opened) => opened,
            None => return Ok(not_found()),
        };
        self.fill(&mut collected, opened)?;
        let content_type = content_type_for(canonical.extension().and_then(|ext| ext.to_str()));
        Ok(Response::bytes_raw(200, collected.finish()).with_content_type(content_type))
    }

    /// The canonical file this request names inside the served root.
    ///
    /// `None` is every way the request does not name one: an unresolvable path,
    /// an unresolvable root, or a resolved path that escaped the root.
    fn confine(&self) -> Option<PathBuf> {
        let requested = self.base_dir.join(self.file_path.as_ref());
        self.step(StaticFileStep::Canonicalize, || {
            let file = canonical_or_warn(&requested, "requested path")?;
            let root = canonical_or_warn(&self.base_dir, "base directory")?;
            file.starts_with(&root).then_some(file)
        })
    }

    /// What the filesystem says this file holds, as a preflight hint.
    ///
    /// `None` for anything that is not a regular file, which is how a directory
    /// under the root becomes a 404 rather than a read that fails.
    fn declared(&self, canonical: &Path) -> Option<u64> {
        self.step(StaticFileStep::Metadata, || {
            match std::fs::metadata(canonical) {
                Ok(metadata) if metadata.is_file() => Some(metadata.len()),
                Ok(_) => None,
                Err(error) => {
                    tracing::warn!(
                        path = %canonical.display(),
                        %error,
                        "static file measurement failed",
                    );
                    None
                }
            }
        })
    }

    /// Open this file, or report why it could not be opened.
    fn open(&self, canonical: &Path) -> Option<std::fs::File> {
        match std::fs::File::open(canonical) {
            Ok(opened) => Some(opened),
            Err(error) => {
                tracing::warn!(
                    path = %canonical.display(),
                    %error,
                    "static file open failed",
                );
                None
            }
        }
    }

    /// Read `opened` into `collected`, one window at a time.
    ///
    /// The window is reused, so the bytes that cross the maximum are left in a
    /// buffer this worker overwrites and drops rather than copied into the
    /// answer first.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] on the window that carries the
    /// total past the maximum, and [`RuntimeError::Io`] when the file cannot be
    /// read.
    fn fill(
        &self,
        collected: &mut CheckedCollector,
        mut opened: std::fs::File,
    ) -> Result<(), RuntimeError> {
        self.step(StaticFileStep::Read, || {
            let mut window = vec![0_u8; read_window(collected.ceiling())].into_boxed_slice();
            loop {
                match opened.read(&mut window) {
                    Ok(0) => return Ok(()),
                    Ok(read) => collected.retain_slice(&window[..read])?,
                    // A signal reached this thread; the file was not read and
                    // was not refused. The profiling feature arms a
                    // process-wide `SIGPROF` timer that fires on every thread,
                    // so a hand-rolled loop that treated this as an IO failure
                    // would answer 500 for a readable file under exactly the
                    // load an operator turns the profiler on for.
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(RuntimeError::Io(error)),
                }
            }
        })
    }
}

/// How large a window one read step takes from a file at a time.
///
/// Sized from the ceiling this read is measured against, never from what the
/// file stated. Metadata and open are separate syscalls on separate paths, so a
/// file replaced between them states a length that no longer describes it, and
/// a window sized from that length reads a large file in thousands of tiny
/// steps on a blocking thread.
///
/// No larger than the ceiling, so a file that grew after it was measured is
/// still refused on the step after the last one the ceiling covered: the first
/// window is admitted whole and the one behind it carries the total past the
/// maximum. Never larger than [`READ_WINDOW`], because the ceiling may be the
/// opt-out's maximum and is not an allocation to make, and never zero, because
/// a zero-length read never ends.
fn read_window(ceiling: usize) -> usize {
    ceiling.clamp(1, READ_WINDOW)
}

/// Canonicalize one path, reporting the resolve failure operators need.
fn canonical_or_warn(path: &Path, resolving: &'static str) -> Option<PathBuf> {
    match path.canonicalize() {
        Ok(canonical) => Some(canonical),
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                resolving,
                "static file resolve failed",
            );
            None
        }
    }
}

/// The answer every request that names no servable file receives.
fn not_found() -> Response {
    Response::text_raw(404, "not found")
}

fn content_type_for(ext: Option<&str>) -> &'static str {
    match ext {
        Some("html") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("txt") => "text/plain",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}
