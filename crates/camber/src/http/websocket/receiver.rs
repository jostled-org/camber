use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Receiver;

use super::deadline::ReceiveDeadline;
use super::message::{WsMessage, WsReceive};
use super::terminal::{TerminalState, WsCloseCause};
use crate::RuntimeError;

/// The one receive owner of a direct WebSocket.
///
/// `Send` but not `Clone`, and every operation takes `&mut self`: one frame can
/// only be taken once, so the type system admits one receive at a time and one
/// owner at all. Dropping this half means nothing will consume the connection's
/// inbound frames, which ends the connection.
///
/// A waiting receive is a future that takes a message only in the poll that
/// returns it. Dropping one that is still pending consumes nothing, so the
/// next receive takes that message exactly once.
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
    /// sent. The wait suspends the calling task, needs no clock, and makes
    /// progress on a current-thread runtime as well as a multi-thread one.
    ///
    /// # Errors
    ///
    /// None today. The public contract keeps the `Result` in reserve, but every
    /// way the connection ends is the [`WsReceive::Closed`] answer, not an
    /// error.
    pub async fn recv(&mut self) -> Result<WsReceive, RuntimeError> {
        Ok(self.next_receive().await)
    }

    /// [`Self::recv`], bounded by `timeout`.
    ///
    /// The deadline is fixed once, when this future is first polled, and
    /// re-polling never moves it. The connection is asked before the clock is:
    /// a message already queued, or a connection that has already ended,
    /// answers at once, so a zero `timeout` is an immediate check rather than a
    /// close. Expiry does not end the connection; the next receive can still
    /// take the next message.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::NoRuntime`] when the receive has to wait and no Tokio
    /// runtime is entered to take a clock from, and [`RuntimeError::Timeout`]
    /// when the deadline expires with neither a message nor a closure. The
    /// entered runtime must enable its time driver.
    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<WsReceive, RuntimeError> {
        ReceiveDeadline::after(timeout)
            .bound(self.next_receive())
            .await
    }

    /// The one receive every public receive is built from. It cannot fail.
    ///
    /// A cause that discards the queue is asked before anything else, so a
    /// cancelled connection answers without waiting. After that the queue's
    /// own receive is the whole wait, and it takes a message only in the poll
    /// that returns it: a deadline or a dropped future leaves that message for
    /// the next receive.
    pub(super) async fn next_receive(&mut self) -> WsReceive {
        match self.discarded() {
            Some(cause) => WsReceive::Closed(cause),
            None => {
                let next = self.frames.recv().await;
                self.settle(next)
            }
        }
    }

    /// The cause this connection ended with, when that cause drops whatever is
    /// still queued.
    ///
    /// Asked before every receive, because the queue itself cannot answer it:
    /// a cancelled bridge fixes the cause and lets go of its producers, and the
    /// messages already in the queue would still be handed out by a receive
    /// that only asked the channel. Closing the receiver here stops any further
    /// admission, and the drain after it is what drops the messages already
    /// queued.
    fn discarded(&mut self) -> Option<WsCloseCause> {
        let cause = self.settled_cause()?;
        match cause.discards_queued_messages() {
            true => {
                self.frames.close();
                while self.frames.try_recv().is_ok() {}
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
