//! One child a WebSocket callback hands to `camber::spawn`, seen from the row.
//!
//! Every runtime-authority row in this binary asks the same questions of such a
//! child: did the closure run at all, and was it still running when its runtime
//! counted it. The child holds rather than returns so that a runtime that
//! admitted it is still counting it when that runtime's own completion looks.
//!
//! The rows also share how a callback reaches its spawn: it tells its peer it is
//! about to wait, and spawns only once the peer resumes it. So the spawn is
//! issued across a real suspension, and the peer knows when one has happened.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use camber::RuntimeError;
use camber::http::WsConn;

use crate::common::DIRECTION_DEADLINE;

/// What a resumable callback sends before each wait for its peer.
pub const AWAITING_PEER: &str = "awaiting-the-peer";

/// Tell the peer this callback is about to wait, then wait for its answer.
///
/// A receive queue that closed instead is a connection with nothing left to
/// resume this callback, which is a closed channel to the callback.
pub async fn await_peer(connection: &mut WsConn) -> Result<Box<str>, RuntimeError> {
    connection.send(AWAITING_PEER).await?;
    connection.recv().await.ok_or(RuntimeError::ChannelClosed)
}

/// The row's end of one spawned child.
pub struct SpawnProbe {
    entered: Receiver<()>,
    release: Sender<()>,
    finished: Receiver<()>,
}

impl SpawnProbe {
    /// Whether the closure reported itself running, waiting under the bound for
    /// it to.
    ///
    /// A refused spawn never runs its closure, so an answer here is itself the
    /// admission: the runtime that took the spawn is the one running it. The
    /// answer is a value rather than an assertion because what a missing one
    /// means belongs to the row — this type does not know which runtime, or
    /// which absence of one, its callback was serving under.
    pub fn entered(&self) -> bool {
        self.entered.recv_timeout(DIRECTION_DEADLINE).is_ok()
    }

    /// Whether the closure has reported running, without waiting for it to.
    ///
    /// Read only after the spawn's refusal is already in hand: a refusal drops
    /// the closure unrun, so the answer is settled rather than raced.
    pub fn never_ran(&self) -> bool {
        self.entered.try_recv().is_err()
    }

    /// Let a running closure leave.
    ///
    /// Nothing to release if the closure never ran, so a closed gate is an
    /// answer, not a failure.
    pub fn release(&self) {
        let _ = self.release.send(());
    }

    /// Release the closure and require it to finish.
    pub fn release_and_finish(&self) {
        self.release();
        self.finished
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("the callback's admitted child never finished");
    }
}

/// The callback's end of one [`SpawnProbe`].
///
/// Clonable, and every closure is built fresh from it, because the callback
/// that spawns the child is an `Fn`: each future owns what it reports through.
#[derive(Clone)]
pub struct ChildParts {
    entered: Sender<()>,
    release: Arc<Mutex<Receiver<()>>>,
    finished: Sender<()>,
}

impl ChildParts {
    pub fn new() -> (Self, SpawnProbe) {
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (finished, finished_rx) = std::sync::mpsc::channel();
        (
            Self {
                entered,
                release: Arc::new(Mutex::new(release_rx)),
                finished,
            },
            SpawnProbe {
                entered: entered_rx,
                release,
                finished: finished_rx,
            },
        )
    }

    /// One spawn's closure: report entry, hold, report completion, answer.
    ///
    /// `camber::spawn` runs the closure on Tokio's blocking pool, so the hold is
    /// a blocking wait on the probe's release — or on the probe going away,
    /// which releases it just the same.
    pub fn body<T: Send + 'static>(
        &self,
        answer: T,
    ) -> impl FnOnce() -> T + Send + 'static + use<T> {
        let entered = self.entered.clone();
        let release = Arc::clone(&self.release);
        let finished = self.finished.clone();
        move || {
            let _ = entered.send(());
            let _ = release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv();
            let _ = finished.send(());
            answer
        }
    }
}
