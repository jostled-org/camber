//! The caller's one handle to a streamed body.

use super::owner::BodyAccess;
use super::state::{Answer, Shared};
use crate::RuntimeError;
use bytes::Bytes;
use tokio::task::JoinHandle;

/// The bare-Tokio task that owns one streamed response.
///
/// Whoever waits for the response or holds its body owns the task. The drop
/// aborts the task, so no dropped waiter or handle leaves a detached owner.
pub(in crate::http::client) struct BareOwner(JoinHandle<()>);

impl BareOwner {
    pub(in crate::http::client) fn of(task: JoinHandle<()>) -> Self {
        Self(task)
    }

    /// Wait until the task is gone.
    ///
    /// The join result is not read. An owner that was aborted or that
    /// panicked already left its terminal fact in the shared state.
    async fn joined(&mut self) {
        let _ = (&mut self.0).await;
    }
}

impl Drop for BareOwner {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// One placed demand, until it is answered.
///
/// A demand dropped before its answer abandons the body. The chunk that
/// answer held is then released and is never handed to a later demand.
struct Demand<'a> {
    shared: &'a Shared,
    answered: bool,
}

impl<'a> Demand<'a> {
    fn place(shared: &'a Shared) -> Self {
        shared.place_demand();
        Self {
            shared,
            answered: false,
        }
    }

    /// Wait for the answer, and mark the demand answered.
    async fn answer(mut self) -> Answer {
        let answer = std::future::poll_fn(|context| self.shared.poll_answer(context)).await;
        self.answered = true;
        answer
    }
}

impl Drop for Demand<'_> {
    fn drop(&mut self) {
        match self.answered {
            true => {}
            false => self.shared.abandon(),
        }
    }
}

/// The body of one streamed outbound response.
///
/// [`ClientStreamResponse::into_body`](crate::http::ClientStreamResponse::into_body)
/// returns it. A body has one handle. The handle is `Send`, and it is not
/// `Clone`. Read the body with [`next`](Self::next), one chunk for each call.
/// Camber reads the upstream only when a call asks, and it holds at most one
/// chunk that the caller has not taken.
///
/// ```compile_fail
/// fn duplicate(body: camber::http::ClientBody) {
///     let _copy = body.clone();
/// }
/// ```
///
/// It is not an inbound [`Response`](crate::http::Response) and does not
/// convert into one. A handler that forwards the body reads each chunk
/// itself.
///
/// ```compile_fail
/// fn forward(body: camber::http::ClientBody) -> camber::http::HandlerOutcome {
///     camber::http::IntoResponse::into_response(body)
/// }
/// ```
///
/// The body ends once. A clean end, a failure, and a cancellation are each
/// final, and every later call returns the same answer. The upstream is not
/// read again.
///
/// Dropping the handle cancels the body. The drop does not wait: use
/// [`close`](Self::close) to wait until the upstream is released.
///
/// One task owns the upstream. Under a Camber runtime that task belongs to
/// the runtime that first polled the send, and that runtime joins it. The
/// handle can move to another task or another runtime, and the owner stays
/// where it is. Outside a Camber runtime the handle owns the task, and its
/// drop aborts it.
pub struct ClientBody {
    /// Declared first, so the handle abandons the body before it aborts the
    /// task.
    access: BodyAccess,
    /// The task, when no Camber runtime owns it.
    owner: Option<BareOwner>,
}

impl ClientBody {
    /// The handle of a body whose owner runs in the bare-Tokio `task`.
    ///
    /// The handle owns the task. No Camber runtime is read or made.
    pub(super) fn bare(access: BodyAccess, task: JoinHandle<()>) -> Self {
        Self::owned(access, Some(BareOwner::of(task)))
    }

    /// The handle of a body under `owner`, or under the Camber runtime that
    /// admitted its task when `owner` is `None`.
    pub(in crate::http::client) fn owned(access: BodyAccess, owner: Option<BareOwner>) -> Self {
        Self { access, owner }
    }

    /// Return the next chunk of the body.
    ///
    /// `Ok(Some(chunk))` is one nonempty chunk, as the upstream sent it.
    /// `Ok(None)` is the clean end of the body. Trailers and empty frames are
    /// not returned.
    ///
    /// The response idle interval starts when the body owner accepts this
    /// call. Time the caller spends between calls is not idle time. It still
    /// counts against the attempt and the operation.
    ///
    /// If the returned future is dropped before it finishes, the body is
    /// cancelled. Later calls return `RuntimeError::Cancelled`.
    ///
    /// # Errors
    ///
    /// - `ClientError::ResponseTooLarge` when the next chunk would pass the
    ///   response maximum. The chunk is not returned, in whole or in part.
    /// - `ClientError::Timeout(ClientTimeout::ResponseIdle)` when no nonempty
    ///   chunk arrives within the idle interval.
    /// - `ClientError::Timeout` with `Attempt` or `Operation` when that
    ///   deadline expires, during a call or between calls.
    /// - `ClientError::Transport` when a read fails or the body is truncated.
    /// - `RuntimeError::Cancelled` after runtime shutdown or after the caller
    ///   cancelled the body.
    ///
    /// An error or `Ok(None)` is returned after the upstream is released.
    pub async fn next(&mut self) -> Result<Option<Bytes>, RuntimeError> {
        match Demand::place(self.access.shared()).answer().await {
            Answer::Chunk(chunk) => Ok(Some(chunk)),
            Answer::Ended(terminal) => terminal.demanded(),
        }
    }

    /// Cancel the body and wait until the upstream is released.
    ///
    /// # Errors
    ///
    /// Returns the failure the body already ended on. A close of a live body
    /// and a close after a clean end return `Ok`.
    pub async fn close(mut self) -> Result<(), RuntimeError> {
        let shared = self.access.shared();
        shared.abandon();
        let terminal = std::future::poll_fn(|context| shared.poll_settled(context)).await;
        if let Some(owner) = self.owner.as_mut() {
            owner.joined().await;
        }
        terminal.closed()
    }
}

impl std::fmt::Debug for ClientBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBody").finish_non_exhaustive()
    }
}
