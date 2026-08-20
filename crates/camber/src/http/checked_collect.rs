//! One checked rule for every buffered collection Camber performs.
//!
//! An outbound client response, a buffered upstream response, and a file read
//! all answer the same question: how many bytes may this process retain from a
//! source it does not control? Stating the answer once, here, is what keeps a
//! ceiling from being a number each consumer re-derives — and what keeps the
//! frame that crosses one from being retained anywhere.
//!
//! Three rules, in the order a collection meets them:
//!
//! - a trustworthy declaration above the maximum fails before anything is
//!   allocated, because a source that states its own size is telling the truth
//!   about work not worth starting;
//! - every chunk is counted with checked addition *before* it is retained, so
//!   the crossing chunk is dropped while it is still only a value this module
//!   holds;
//! - an arithmetic overflow is the same answer as a crossing, because a wrapped
//!   small total is a bound that silently stopped applying.
//!
//! The arithmetic itself is [`body_admission`](super::body_admission)'s, not a
//! second copy: a request body and a client response that disagreed about what
//! "one byte past the maximum" means would be two bounds wearing one name.

use super::body_admission::{checked_body_frame_total, declared_length_exceeds_limit};
use super::boundary::ByteBoundary;
use super::mock::LifecycleScript;
use crate::RuntimeError;
use bytes::{Bytes, BytesMut};
use std::sync::Arc;

/// The bound one buffered collection retains under.
///
/// Not cloneable and not copyable: the running total and the bytes behind it
/// are one owner's, and a second handle to them would be a second answer about
/// how much this collection has already kept.
pub(super) struct CheckedCollector {
    /// The maximum this collection names when it refuses.
    boundary: ByteBoundary,
    /// The configured maximum, or `None` for an explicit opt-out.
    limit: Option<usize>,
    retained: BytesMut,
    /// The peer-scoped observer that records what was polled and kept, when a
    /// test registered one. Inert otherwise.
    observer: Option<Arc<LifecycleScript>>,
}

impl CheckedCollector {
    /// Start a collection under `limit`, reporting crossings as `boundary`.
    pub(super) fn new(
        boundary: ByteBoundary,
        limit: Option<usize>,
        observer: Option<Arc<LifecycleScript>>,
    ) -> Self {
        Self {
            boundary,
            limit,
            retained: BytesMut::new(),
            observer,
        }
    }

    /// The ceiling this collection measures against.
    ///
    /// An opt-out is the largest total the platform can represent rather than a
    /// second code path: one comparison then serves both, and an unbounded
    /// collection still fails on an overflowing total instead of wrapping.
    ///
    /// Readable by the module a collection belongs to, so an owner reporting
    /// which maximum it froze reports the number this comparison uses rather
    /// than a copy of the argument it passed in.
    pub(super) fn ceiling(&self) -> usize {
        self.limit.unwrap_or(usize::MAX)
    }

    /// The refusal this collection reports, naming the maximum it crossed.
    fn crossed(&self) -> RuntimeError {
        RuntimeError::LimitExceeded(self.boundary)
    }

    /// Refuse a source that states a size above the maximum, before allocating.
    ///
    /// The declaration is compared at its own `u64` width, so a length no
    /// machine could hold cannot narrow into a small admitted number.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] naming this collection's
    /// boundary when the stated length is above the configured maximum.
    pub(super) fn admit_declared(&self, declared: Option<u64>) -> Result<(), RuntimeError> {
        match declared.is_some_and(|stated| declared_length_exceeds_limit(stated, self.ceiling())) {
            true => Err(self.crossed()),
            false => Ok(()),
        }
    }

    /// Buy the buffer a source's admitted declaration says it will need.
    ///
    /// The declaration was read to refuse an over-large source, and then it was
    /// worth nothing more: an empty `BytesMut` grows by doubling from sixty-four
    /// bytes, so a declared eight MiB answer pays about seventeen reallocations
    /// and seventeen copies for a size the source already stated.
    ///
    /// Called after [`Self::admit_declared`] has accepted the declaration, so
    /// nothing is bought for a length this collection has already refused.
    ///
    /// The reserve is what [`declared_reserve`] permits, never what the source
    /// asked for. A declaration is an untrusted peer claim, and under an
    /// unbounded ceiling an unclamped reserve would be an allocation any peer
    /// could name: the cap is what makes reserving from a peer's number safe.
    pub(super) fn reserve_declared(&mut self, declared: Option<u64>) {
        self.retained
            .reserve(declared_reserve(declared, self.ceiling()));
    }

    /// Count one chunk, then keep it — or drop it and refuse.
    ///
    /// The chunk is taken by value so that a crossing one is released here,
    /// where the refusal is decided, rather than left for a caller to remember
    /// to drop.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] naming this collection's
    /// boundary when the chunk would carry the total past the maximum, or when
    /// the addition that would prove otherwise overflows.
    pub(super) fn retain(&mut self, chunk: Bytes) -> Result<(), RuntimeError> {
        self.retain_slice(&chunk)
    }

    /// Count one chunk this collection does not own, then copy it — or refuse.
    ///
    /// The same three rules as [`Self::retain`], for a source that hands out a
    /// borrowed window onto a buffer it reuses: a file read fills one buffer
    /// over and over, and a crossing window is left in that buffer for its
    /// owner to overwrite rather than copied here first.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::LimitExceeded`] naming this collection's
    /// boundary when the chunk would carry the total past the maximum, or when
    /// the addition that would prove otherwise overflows.
    pub(super) fn retain_slice(&mut self, chunk: &[u8]) -> Result<(), RuntimeError> {
        LifecycleScript::count_collected_chunk(self.observer.as_deref());
        let admitted =
            checked_body_frame_total(self.retained.len(), chunk.len(), self.ceiling()).is_some();
        if admitted {
            self.retained.extend_from_slice(chunk);
        }
        // Read from the buffer, after the decision, on both paths: a collector
        // that kept a crossing chunk before refusing it would report the bytes
        // it is holding, where a total it was permitted would hide them.
        LifecycleScript::observe_collected_retained(self.observer.as_deref(), self.retained.len());
        match admitted {
            true => Ok(()),
            false => Err(self.crossed()),
        }
    }

    /// Freeze what was retained into the shared buffer its owner receives.
    pub(super) fn finish(self) -> Bytes {
        self.retained.freeze()
    }
}

/// The most a peer's declaration may buy in advance (64 KiB).
///
/// A source states its own size, and this process is the one that pays for
/// believing it. Past this, a body that keeps arriving buys its own buffer out
/// of bytes that actually arrived.
const MAX_DECLARED_RESERVE: usize = 64 * 1024;

/// The bytes one admitted declaration buys before the first chunk is read.
///
/// Three clamps, and each removes a different way a peer's number could name an
/// allocation: the platform word, because a declaration is a `u64` and no
/// machine holds every one of them; this collection's own ceiling, because
/// nothing above it would be kept; and [`MAX_DECLARED_RESERVE`], because an
/// unbounded ceiling has no number of its own to clamp against.
///
/// `None` is a source that stated no size, which buys nothing.
fn declared_reserve(declared: Option<u64>, ceiling: usize) -> usize {
    declared.map_or(0, |stated| {
        usize::try_from(stated)
            .unwrap_or(usize::MAX)
            .min(ceiling)
            .min(MAX_DECLARED_RESERVE)
    })
}

/// The quiet interval one collection allows between the chunks it reads.
///
/// Carried as the interval and the boundary together, because a collection that
/// enforces one owes the operator the name of the deadline that was configured:
/// a buffered upstream answer and an outbound client answer stall under
/// different policies and must not report each other's.
#[derive(Clone, Copy)]
pub(super) struct CollectionIdle {
    interval: std::time::Duration,
    boundary: super::boundary::DeadlineBoundary,
}

impl CollectionIdle {
    /// The quiet interval one collection enforces, under the name it reports.
    pub(super) const fn new(
        interval: std::time::Duration,
        boundary: super::boundary::DeadlineBoundary,
    ) -> Self {
        Self { interval, boundary }
    }
}

/// Take the next chunk of one answer, under the quiet interval it allows.
///
/// `None` for `idle` is a consumer whose own transport already bounds its
/// reads; nothing here arms a second timer for it.
async fn next_chunk(
    response: &mut reqwest::Response,
    idle: Option<CollectionIdle>,
) -> Result<Option<Bytes>, RuntimeError> {
    let read = match idle {
        None => response.chunk().await,
        Some(idle) => tokio::time::timeout(idle.interval, response.chunk())
            .await
            .map_err(|_| RuntimeError::DeadlineExceeded(idle.boundary))?,
    };
    read.map_err(super::map_reqwest_error)
}

/// Collect one outbound response body under `limit`.
///
/// The shared entry point for every Reqwest-backed consumer: the public client
/// and the buffered proxy read the same declaration, the same chunks, and the
/// same refusal, so a body one of them would refuse is never admitted by the
/// other.
///
/// Nothing is polled after a terminal. The loop returns on the first refused
/// chunk, so the response — and the connection under it — is released with its
/// remaining frames unread.
///
/// # Errors
///
/// Returns [`RuntimeError::LimitExceeded`] naming `boundary` when the body
/// crosses `limit`, [`RuntimeError::DeadlineExceeded`] naming `idle`'s boundary
/// when the source goes quiet for longer than it allows, or the mapped
/// transport failure when the body cannot be read.
pub(super) async fn collect_response(
    mut response: reqwest::Response,
    boundary: ByteBoundary,
    limit: Option<usize>,
    idle: Option<CollectionIdle>,
) -> Result<Bytes, RuntimeError> {
    let observer = response
        .remote_addr()
        .and_then(super::mock::lifecycle_script);
    // Read once and used twice: the same declaration that refuses an over-large
    // source sizes the buffer for one this collection will keep. Reading it a
    // second time would be a second answer about what the peer stated.
    let declared = response.content_length();
    let mut collector = CheckedCollector::new(boundary, limit, observer);
    collector.admit_declared(declared)?;
    collector.reserve_declared(declared);
    while let Some(chunk) = next_chunk(&mut response, idle).await? {
        collector.retain(chunk)?;
    }
    Ok(collector.finish())
}
