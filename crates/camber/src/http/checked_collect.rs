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
    fn ceiling(&self) -> usize {
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
        LifecycleScript::count_collected_chunk(self.observer.as_deref());
        let admitted =
            checked_body_frame_total(self.retained.len(), chunk.len(), self.ceiling()).is_some();
        if admitted {
            self.retained.extend_from_slice(&chunk);
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
/// crosses `limit`, or the mapped transport failure when the body cannot be
/// read.
pub(super) async fn collect_response(
    mut response: reqwest::Response,
    boundary: ByteBoundary,
    limit: Option<usize>,
) -> Result<Bytes, RuntimeError> {
    let observer = response
        .remote_addr()
        .and_then(super::mock::lifecycle_script);
    let mut collector = CheckedCollector::new(boundary, limit, observer);
    collector.admit_declared(response.content_length())?;
    while let Some(chunk) = response.chunk().await.map_err(super::map_reqwest_error)? {
        collector.retain(chunk)?;
    }
    Ok(collector.finish())
}
