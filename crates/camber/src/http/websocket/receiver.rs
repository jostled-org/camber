use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

use super::message::{WsMessage, WsReceive};
use super::terminal::{TerminalState, WsCloseCause};
use crate::RuntimeError;

/// The one receive owner of a direct WebSocket.
///
/// `Send` but not `Clone`, and every operation takes `&mut self`: one frame can
/// only be taken once, so the type system admits one receive at a time and one
/// owner at all. Dropping this half means nothing will consume the connection's
/// inbound frames, which ends the connection.
pub struct WsReceiver {
    frames: Receiver<WsMessage>,
    terminal: Arc<TerminalState>,
}

impl std::fmt::Debug for WsReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsReceiver").finish_non_exhaustive()
    }
}

impl WsReceiver {
    pub(crate) fn new(frames: Receiver<WsMessage>, terminal: Arc<TerminalState>) -> Self {
        Self { frames, terminal }
    }

    /// Take the next application message, or the reason there will not be one.
    ///
    /// Ping and pong frames never arrive here — the transport answers those
    /// itself — so every message this returns is one the peer's application
    /// sent.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::BlockingInAsyncContext`] on a current-thread Tokio
    /// runtime, where waiting would stop the only thread that could deliver a
    /// frame.
    pub fn recv(&mut self) -> Result<WsReceive, RuntimeError> {
        match self.discarded() {
            Some(cause) => Ok(WsReceive::Closed(cause)),
            None => {
                let next = crate::task::wait_blocking(|| self.frames.blocking_recv())?;
                Ok(self.settle(next))
            }
        }
    }

    /// [`Self::recv`], bounded by `timeout`.
    ///
    /// The connection is asked before the runtime is. A connection that has
    /// already ended answers with its cause, exactly as [`Self::recv`] does, so
    /// a receive loop sees the end of the connection on every flavor and off
    /// every runtime. Only a live connection has to wait, so only a live
    /// connection can be refused a clock or a thread to wait on.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::NoRuntime`] with no Tokio runtime to take a clock from,
    /// [`RuntimeError::BlockingInAsyncContext`] on a current-thread runtime,
    /// and [`RuntimeError::Timeout`] when the deadline expires with neither a
    /// message nor a closure.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<WsReceive, RuntimeError> {
        match self.discarded() {
            Some(cause) => Ok(WsReceive::Closed(cause)),
            None => self.timed(timeout),
        }
    }

    /// The cause this connection ended with, when that cause drops whatever is
    /// still queued.
    ///
    /// Asked before every receive, because the queue itself cannot answer it:
    /// a cancelled bridge fixes the cause and lets go of its producers, and the
    /// messages already in the queue would still be handed out by a receive
    /// that only asked the channel. Closing the receiver here is what actually
    /// drops them.
    fn discarded(&mut self) -> Option<WsCloseCause> {
        let cause = self.settled_cause()?;
        match cause.discards_queued_messages() {
            true => {
                self.frames.close();
                Some(cause)
            }
            false => None,
        }
    }

    /// This connection's cause, once there is one.
    ///
    /// A committed cause is the ordinary answer, on every path a server takes
    /// including cancellation. The second arm covers the one path left that
    /// skips the commit: a bridge that outlasts its server's shutdown deadline
    /// is taken away and drops both queue ends without running its exit, which
    /// leaves a closed queue and no cause — and a bridge taken away is what
    /// [`WsCloseCause::ServerCancelled`] means. Reading that through the queue's
    /// own closure is what keeps such a connection's buffered messages from
    /// being handed out as if it had ended in order.
    fn settled_cause(&self) -> Option<WsCloseCause> {
        match (self.terminal.cause(), self.frames.is_closed()) {
            (Some(cause), _) => Some(cause),
            (None, true) => Some(self.terminal.committed()),
            (None, false) => None,
        }
    }

    /// One bounded wait, on a runtime that can afford to serve it.
    ///
    /// The deadline is the entered runtime's clock, so a caller off every
    /// runtime has nothing to bound the wait with. A current-thread runtime has
    /// the clock and not the thread: waiting there would stop the only worker
    /// that could deliver a frame.
    ///
    /// [`crate::task::wait_blocking`] states the same policy for the unbounded
    /// waits and cannot stand in for this one. It runs the wait inline off every
    /// runtime, and this wait has no handle to take a clock from there —
    /// answering `NoRuntime` is the difference, and it is a difference in
    /// behavior rather than in wording.
    fn timed(&mut self, timeout: Duration) -> Result<WsReceive, RuntimeError> {
        let handle = tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
        match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => self.timed_on_worker(&handle, timeout),
            _ => Err(RuntimeError::BlockingInAsyncContext),
        }
    }

    /// One timed receive, run off the poll path of the worker that asked for
    /// it.
    fn timed_on_worker(
        &mut self,
        handle: &tokio::runtime::Handle,
        timeout: Duration,
    ) -> Result<WsReceive, RuntimeError> {
        let next = crate::task::block_in_place(|| {
            handle.block_on(tokio::time::timeout(timeout, self.frames.recv()))
        })
        .map_err(|_| RuntimeError::Timeout)?;
        Ok(self.settle(next))
    }

    /// Turn one queue answer into a public receive answer.
    ///
    /// A closed queue is never an absence: the bridge commits this connection's
    /// cause before it lets the queue go, so the answer here is that cause.
    fn settle(&self, next: Option<WsMessage>) -> WsReceive {
        match next {
            Some(message) => WsReceive::Message(message),
            None => WsReceive::Closed(self.terminal.committed()),
        }
    }
}
