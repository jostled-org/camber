//! The component driver of the body engine.
//!
//! Semver-unsupported. A test uses it to run the production body owner over a
//! scripted source: frames a real peer cannot send on demand. The driver
//! supplies frames and reads counts. It commits no terminal fact, and the
//! engine reads its source as it reads any other.

use super::super::lifetime::Lifetime;
use super::handle::ClientBody;
use super::owner::{BodyBudget, BodyOwner, BodyWatch};
use crate::runtime_state::recover_poisoned;
use bytes::Bytes;
use hyper::body::{Body, Frame};
use hyper::http::HeaderMap;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::task::AbortHandle;

/// The bounds one driven body is read under.
#[derive(Clone, Copy, Debug)]
pub struct ClientBodyBounds {
    /// The most payload bytes the body may deliver.
    pub limit: usize,
    /// The running total the count starts at.
    pub counted: usize,
    /// How long one accepted demand may wait for a nonempty chunk.
    pub idle: Duration,
    /// The operation total, measured from [`ClientBodyDriver::begin`].
    pub total: Duration,
    /// The attempt bound, measured from [`ClientBodyDriver::begin`].
    pub attempt: Option<Duration>,
}

/// One frame the scripted source returns.
#[derive(Debug)]
pub enum ClientBodyFrame {
    /// A data frame. An empty one is framing.
    Data(Bytes),
    /// A trailers frame.
    Trailers,
    /// A failed read. The engine names its category.
    Failure,
    /// The clean end of the body.
    End,
}

/// What the source and its owner did, as a copy of the counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientBodyObservation {
    /// Every poll of the source, pending ones included.
    pub source_polls: usize,
    /// Frames the source returned, a failure and the end included.
    pub source_frames: usize,
    /// Nonempty chunks that passed the byte check.
    pub admitted_chunks: usize,
    /// The bytes of those chunks.
    pub admitted_bytes: usize,
    /// Admitted chunks the caller has not taken.
    pub retained_chunks: usize,
    /// The bytes of those chunks.
    pub retained_bytes: usize,
    /// The most `retained_bytes` has been.
    pub peak_retained_bytes: usize,
    /// The owner accepted a demand and armed its idle expiry. False again
    /// once the demand is answered.
    pub pending_demand: bool,
    /// The owner waits with no demand, under its carried expiries.
    pub awaiting_demand: bool,
    /// The source is dropped, by any path.
    pub source_released: bool,
    /// The owner task's future is dropped.
    pub owner_ended: bool,
}

/// What one source and the task that owns it did.
#[derive(Clone, Copy, Default)]
pub(in crate::http::client) struct SourceCounts {
    /// Every poll of the source, pending ones included.
    pub(in crate::http::client) polls: usize,
    /// Frames the source returned, a failure and the end included.
    pub(in crate::http::client) frames: usize,
    /// The source is dropped, by any path.
    pub(in crate::http::client) released: bool,
    /// The owner task's future is dropped.
    pub(in crate::http::client) owner_ended: bool,
}

impl ClientBodyObservation {
    /// One copy of what a source did and what `watch` counted beside it. A
    /// source with no body yet has counted nothing.
    pub(in crate::http::client) fn of(source: SourceCounts, watch: Option<&BodyWatch>) -> Self {
        let counts = watch.map(BodyWatch::counts).unwrap_or_default();
        Self {
            source_polls: source.polls,
            source_frames: source.frames,
            admitted_chunks: counts.admitted_chunks,
            admitted_bytes: counts.admitted_bytes,
            retained_chunks: counts.retained_chunks,
            retained_bytes: counts.retained_bytes,
            peak_retained_bytes: counts.peak_retained_bytes,
            pending_demand: counts.pending_demand,
            awaiting_demand: counts.awaiting_demand,
            source_released: source.released,
            owner_ended: source.owner_ended,
        }
    }
}

/// What the script holds and what its source did.
#[derive(Default)]
struct Script {
    frames: VecDeque<ClientBodyFrame>,
    /// The waker of a source poll that found nothing queued.
    reader: Option<Waker>,
    counts: SourceCounts,
}

/// The script one driver and its source share.
#[derive(Default)]
struct Stage {
    script: Mutex<Script>,
}

impl Stage {
    /// Lock the script. No holder can panic with it half-written.
    fn script(&self) -> MutexGuard<'_, Script> {
        recover_poisoned(self.script.lock())
    }
}

/// The failed read of a scripted source. It holds nothing.
struct ScriptedFailure;

/// The source the engine reads: the queued frames, in order.
///
/// A poll with nothing queued stays pending until a frame is fed. The drop
/// is the release: it discards every queued frame.
struct ScriptedSource(Arc<Stage>);

impl Body for ScriptedSource {
    type Data = Bytes;
    type Error = ScriptedFailure;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, ScriptedFailure>>> {
        let mut script = self.0.script();
        script.counts.polls = script.counts.polls.saturating_add(1);
        let Some(frame) = script.frames.pop_front() else {
            script.reader = Some(context.waker().clone());
            return Poll::Pending;
        };
        script.counts.frames = script.counts.frames.saturating_add(1);
        Poll::Ready(match frame {
            ClientBodyFrame::Data(data) => Some(Ok(Frame::data(data))),
            ClientBodyFrame::Trailers => Some(Ok(Frame::trailers(HeaderMap::new()))),
            ClientBodyFrame::Failure => Some(Err(ScriptedFailure)),
            ClientBodyFrame::End => None,
        })
    }
}

impl Drop for ScriptedSource {
    fn drop(&mut self) {
        let mut script = self.0.script();
        script.counts.released = true;
        script.frames.clear();
        script.reader = None;
    }
}

/// The witness that the owner task's future is dropped.
struct OwnerEnded(Arc<Stage>);

impl Drop for OwnerEnded {
    fn drop(&mut self) {
        self.0.script().counts.owner_ended = true;
    }
}

/// Run `owner` to its end as the whole of one task.
///
/// The witness is dropped after the owner, on every path: at the return, and
/// when the task is aborted.
async fn owned(ended: OwnerEnded, owner: BodyOwner<ScriptedSource>) {
    owner.run().await;
    drop(ended);
}

/// A source with its bounds fixed, before the handoff.
struct Fixed {
    source: ScriptedSource,
    budget: BodyBudget,
}

/// What the driver keeps of a body it handed off.
struct HandedOff {
    watch: BodyWatch,
    owner: AbortHandle,
}

/// Drives the production body owner over a scripted source.
///
/// Semver-unsupported.
pub struct ClientBodyDriver {
    stage: Arc<Stage>,
    fixed: Mutex<Option<Fixed>>,
    handed_off: OnceLock<HandedOff>,
}

impl ClientBodyDriver {
    /// Fix the operation deadline and the attempt expiry now.
    ///
    /// An operation fixes both before its body exists: the deadline at its
    /// first poll, and the attempt expiry at its final send. The handoff
    /// moves them and restarts neither.
    ///
    /// A total that is already spent ends the operation here. Such a driver
    /// has no body to hand off, and its source is released.
    pub fn begin(bounds: ClientBodyBounds) -> Self {
        let stage = Arc::new(Stage::default());
        let source = ScriptedSource(Arc::clone(&stage));
        let fixed = Lifetime::begin(bounds.total, None)
            .ok()
            .map(|lifetime| Fixed {
                source,
                budget: BodyBudget {
                    limit: bounds.limit,
                    counted: bounds.counted,
                    idle: bounds.idle,
                    attempt: lifetime.attempt_bound(bounds.attempt),
                    lifetime,
                },
            });
        Self {
            stage,
            fixed: Mutex::new(fixed),
            handed_off: OnceLock::new(),
        }
    }

    /// Start one bare-Tokio task that runs the engine over the source, and
    /// return the body handle.
    ///
    /// `None` after the first call, outside a Tokio runtime, and for a
    /// driver whose total was spent at [`begin`](Self::begin).
    pub fn hand_off(&self) -> Option<ClientBody> {
        let executor = tokio::runtime::Handle::try_current().ok()?;
        let Fixed { source, budget } = recover_poisoned(self.fixed.lock()).take()?;
        let (owner, access) = BodyOwner::new(source, budget, None);
        let task = executor.spawn(owned(OwnerEnded(Arc::clone(&self.stage)), owner));
        self.handed_off.get_or_init(|| HandedOff {
            watch: access.watch(),
            owner: task.abort_handle(),
        });
        Some(ClientBody::bare(access, task))
    }

    /// Queue the next source frame, and wake a source that waits for it.
    ///
    /// A released source keeps no frame: the frame is dropped here.
    pub fn feed(&self, frame: ClientBodyFrame) {
        let reader = {
            let mut script = self.stage.script();
            match script.counts.released {
                true => None,
                false => {
                    script.frames.push_back(frame);
                    script.reader.take()
                }
            }
        };
        if let Some(reader) = reader {
            reader.wake();
        }
    }

    /// Abort the owner task. The abort names no result.
    pub fn abort_owner(&self) {
        if let Some(handed_off) = self.handed_off.get() {
            handed_off.owner.abort();
        }
    }

    /// A copy of the counts.
    pub fn observed(&self) -> ClientBodyObservation {
        let watch = self.handed_off.get().map(|handed_off| &handed_off.watch);
        let source = self.stage.script().counts;
        ClientBodyObservation::of(source, watch)
    }
}

impl std::fmt::Debug for ClientBodyDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBodyDriver")
            .field("observed", &self.observed())
            .finish()
    }
}
