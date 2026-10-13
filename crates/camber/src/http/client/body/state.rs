//! The state one body owner and its handle share.
//!
//! One mutex guards it. No guard is held across a wait, and each side keeps
//! one waker here. The state holds at most one demand and at most one chunk.
//!
//! The terminal fact is written once. The first writer commits it, and every
//! later writer reads it. The owner's settlement then marks it settled, after
//! the owner dropped its source. A caller reads a terminal fact only when it
//! is settled, so the fact is also the acknowledgement of that release.

use super::super::completion::Terminal;
use super::super::error::ClientError;
use crate::RuntimeError;
use crate::runtime_state::recover_poisoned;
use bytes::Bytes;
use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

/// The fact one body ended on.
#[derive(Clone, Copy)]
pub(super) enum BodyTerminal {
    /// The source ended cleanly.
    End,
    /// The caller closed the body, dropped it, or dropped a pending demand.
    Abandoned,
    /// The runtime shut down.
    Stopped,
    /// The body failed with this category.
    Failed(ClientError),
}

impl BodyTerminal {
    /// What a demand returns after this fact.
    ///
    /// A demand after an abandonment reports the cancellation. It never
    /// reports a clean end, because the caller did not read one.
    pub(super) fn demanded(self) -> Result<Option<Bytes>, RuntimeError> {
        match self {
            Self::End => Ok(None),
            Self::Abandoned | Self::Stopped => Err(RuntimeError::Cancelled),
            Self::Failed(category) => Err(RuntimeError::Client(category)),
        }
    }

    /// What a close returns after this fact.
    ///
    /// A close is the caller's own cancellation, so an abandonment is its
    /// success. A cause committed before the close stays an error.
    pub(super) fn closed(self) -> Result<(), RuntimeError> {
        match self {
            Self::End | Self::Abandoned => Ok(()),
            Self::Stopped => Err(RuntimeError::Cancelled),
            Self::Failed(category) => Err(RuntimeError::Client(category)),
        }
    }

    /// The fact the operation's terminal record states for this end.
    pub(super) const fn recorded(self) -> Terminal {
        match self {
            Self::End => Terminal::ANSWERED,
            Self::Abandoned => Terminal::ABANDONED,
            Self::Stopped => Terminal::CANCELLED,
            Self::Failed(category) => Terminal::failed(category),
        }
    }
}

/// The fact of a body whose owner ended without a committed cause.
///
/// The owner was aborted or it panicked. The caller learns only that the
/// transfer failed.
const OWNER_LOST: BodyTerminal = BodyTerminal::Failed(ClientError::Transport);

/// How far the body is from its end.
#[derive(Clone, Copy)]
enum Phase {
    /// No terminal fact is committed.
    Live,
    /// The fact is committed. The owner still holds its source.
    Committed(BodyTerminal),
    /// The fact is committed and the source is released.
    Settled(BodyTerminal),
}

impl Phase {
    /// The committed fact, or `None` for a live body.
    const fn committed(self) -> Option<BodyTerminal> {
        match self {
            Self::Live => None,
            Self::Committed(terminal) | Self::Settled(terminal) => Some(terminal),
        }
    }
}

/// What the owner did, as plain counts. Nothing reads them to decide.
#[derive(Clone, Copy, Default)]
pub(super) struct BodyCounts {
    /// Nonempty chunks that passed the byte check.
    pub(super) admitted_chunks: usize,
    /// The bytes of those chunks.
    pub(super) admitted_bytes: usize,
    /// Admitted chunks the caller has not taken: zero or one.
    pub(super) retained_chunks: usize,
    /// The bytes of that chunk.
    pub(super) retained_bytes: usize,
    /// The most `retained_bytes` has been.
    pub(super) peak_retained_bytes: usize,
    /// The owner accepted a demand and has not answered it.
    pub(super) pending_demand: bool,
    /// The owner waits with no demand.
    pub(super) awaiting_demand: bool,
}

/// What one side of the body received for a demand.
pub(super) enum Answer {
    /// The one admitted chunk.
    Chunk(Bytes),
    /// The settled terminal fact.
    Ended(BodyTerminal),
}

/// Everything the mutex guards.
struct Slot {
    phase: Phase,
    /// The caller placed a demand and the owner has not accepted it.
    demanded: bool,
    /// The one admitted chunk the caller has not taken.
    chunk: Option<Bytes>,
    owner: Option<Waker>,
    caller: Option<Waker>,
    counts: BodyCounts,
}

impl Slot {
    /// Commit `terminal` on a live body, and return the fact in force.
    ///
    /// The fact is written once: a committed fact is kept and returned.
    fn commit(&mut self, terminal: BodyTerminal) -> BodyTerminal {
        match self.phase.committed() {
            Some(kept) => kept,
            None => {
                self.phase = Phase::Committed(terminal);
                terminal
            }
        }
    }
}

/// Wake `waker` after its lock is released.
fn wake(waker: Option<Waker>) {
    if let Some(waker) = waker {
        waker.wake();
    }
}

/// The state one body owner and its handle share.
pub(super) struct Shared {
    slot: Mutex<Slot>,
}

impl Shared {
    pub(super) fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                phase: Phase::Live,
                demanded: false,
                chunk: None,
                owner: None,
                caller: None,
                counts: BodyCounts::default(),
            }),
        }
    }

    /// Lock the state.
    ///
    /// No holder runs code that can panic with the state half-written, so a
    /// poisoned lock still guards a consistent value.
    fn slot(&self) -> MutexGuard<'_, Slot> {
        recover_poisoned(self.slot.lock())
    }

    /// The counts, with the retained chunk read from the slot itself.
    pub(super) fn counts(&self) -> BodyCounts {
        let slot = self.slot();
        BodyCounts {
            retained_chunks: usize::from(slot.chunk.is_some()),
            retained_bytes: slot.chunk.as_ref().map_or(0, Bytes::len),
            ..slot.counts
        }
    }

    /// Caller: place one demand on a live body.
    ///
    /// A body with a committed fact takes no demand. The caller then reads
    /// that fact.
    pub(super) fn place_demand(&self) {
        let waker = {
            let mut slot = self.slot();
            match slot.phase {
                Phase::Live => {
                    slot.demanded = true;
                    slot.owner.take()
                }
                Phase::Committed(_) | Phase::Settled(_) => None,
            }
        };
        wake(waker);
    }

    /// Caller: take the answer to the placed demand.
    ///
    /// A waiting chunk is the answer. A terminal fact is the answer only when
    /// it is settled, so the caller that reads it knows the source is gone.
    pub(super) fn poll_answer(&self, context: &mut Context<'_>) -> Poll<Answer> {
        let mut slot = self.slot();
        match (slot.chunk.take(), slot.phase) {
            (Some(chunk), _) => Poll::Ready(Answer::Chunk(chunk)),
            (None, Phase::Settled(terminal)) => Poll::Ready(Answer::Ended(terminal)),
            (None, Phase::Live | Phase::Committed(_)) => {
                slot.caller = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Caller: wait for the settled terminal fact.
    pub(super) fn poll_settled(&self, context: &mut Context<'_>) -> Poll<BodyTerminal> {
        let mut slot = self.slot();
        match slot.phase {
            Phase::Settled(terminal) => Poll::Ready(terminal),
            Phase::Live | Phase::Committed(_) => {
                slot.caller = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Caller: abandon the body.
    ///
    /// A live body commits the abandonment here, before the owner is told. A
    /// chunk that waits is released with it, so no later demand receives a
    /// chunk whose delivery was interrupted. A committed fact is not changed.
    pub(super) fn abandon(&self) {
        let waker = {
            let mut slot = self.slot();
            match slot.phase {
                Phase::Live => {
                    slot.phase = Phase::Committed(BodyTerminal::Abandoned);
                    slot.demanded = false;
                    slot.chunk = None;
                    slot.owner.take()
                }
                Phase::Committed(_) | Phase::Settled(_) => None,
            }
        };
        wake(waker);
    }

    /// Owner: wait for the next demand, and accept it.
    ///
    /// `Err` is a fact the caller committed. The owner stops on it.
    pub(super) fn poll_demand(&self, context: &mut Context<'_>) -> Poll<Result<(), BodyTerminal>> {
        let mut slot = self.slot();
        match (slot.phase.committed(), slot.demanded) {
            (Some(terminal), _) => Poll::Ready(Err(terminal)),
            (None, true) => {
                slot.demanded = false;
                slot.counts.awaiting_demand = false;
                slot.counts.pending_demand = true;
                Poll::Ready(Ok(()))
            }
            (None, false) => {
                slot.counts.awaiting_demand = true;
                slot.owner = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Owner: wait for a fact the caller commits under an accepted demand.
    pub(super) fn poll_withdrawn(&self, context: &mut Context<'_>) -> Poll<BodyTerminal> {
        let mut slot = self.slot();
        match slot.phase.committed() {
            Some(terminal) => Poll::Ready(terminal),
            None => {
                slot.owner = Some(context.waker().clone());
                Poll::Pending
            }
        }
    }

    /// Owner: answer the accepted demand with one admitted chunk.
    ///
    /// `Err` is a fact the caller committed first. The chunk is dropped here
    /// and is not delivered.
    pub(super) fn deliver(&self, chunk: Bytes) -> Result<(), BodyTerminal> {
        let waker = {
            let mut slot = self.slot();
            if let Some(terminal) = slot.phase.committed() {
                return Err(terminal);
            }
            let counts = &mut slot.counts;
            counts.pending_demand = false;
            counts.admitted_chunks = counts.admitted_chunks.saturating_add(1);
            counts.admitted_bytes = counts.admitted_bytes.saturating_add(chunk.len());
            counts.peak_retained_bytes = counts.peak_retained_bytes.max(chunk.len());
            slot.chunk = Some(chunk);
            slot.caller.take()
        };
        wake(waker);
        Ok(())
    }

    /// Owner: commit `terminal` on a live body. A committed fact is kept.
    pub(super) fn commit(&self, terminal: BodyTerminal) {
        self.slot().commit(terminal);
    }

    /// Owner: fix the fact the body ended on, and return it.
    ///
    /// The owner's settlement calls this once, after the source is dropped
    /// and before it acknowledges. A body that is still live lost its owner,
    /// and that is committed here, so no later writer changes the fact the
    /// settlement publishes.
    pub(super) fn conclude(&self) -> BodyTerminal {
        self.slot().commit(OWNER_LOST)
    }

    /// Owner: acknowledge that the source is released.
    ///
    /// The owner's settlement calls this once, after the source is dropped,
    /// with the fact `conclude` returned. The waiting chunk is released before
    /// the fact is marked settled, so a caller that reads the fact finds
    /// nothing retained.
    pub(super) fn settle(&self, terminal: BodyTerminal) {
        let waker = {
            let mut slot = self.slot();
            slot.chunk = None;
            slot.phase = Phase::Settled(terminal);
            slot.demanded = false;
            slot.counts.pending_demand = false;
            slot.counts.awaiting_demand = false;
            slot.owner = None;
            slot.caller.take()
        };
        wake(waker);
    }
}
