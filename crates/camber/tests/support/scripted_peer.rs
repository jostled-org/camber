//! Plumbing the scripted loopback peers share: a nonblocking loopback
//! listener, an accept loop that polls until the peer stops or its listener
//! fails, tracked threads and a bounded join that counts the ones still
//! running or panicked, the threads one peer owns and the stop they watch, a
//! poison-tolerant lock, the condvar-backed state a peer's threads and its
//! row share, and the control a row scripts and reads a peer through.

use crate::integration_rows::{Row, expect_eq};
use std::fmt::Debug;
use std::io::{self, ErrorKind};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The threads a peer spawned to serve its connections.
pub type Spawned = Mutex<Vec<JoinHandle<()>>>;

/// Bind a nonblocking listener on an ephemeral loopback port.
///
/// # Errors
///
/// When the listener cannot bind, turn nonblocking, or report its address.
pub fn bind_loopback() -> Result<(TcpListener, SocketAddr), String> {
    let listener =
        TcpListener::bind("127.0.0.1:0").map_err(|error| format!("bind loopback: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("loopback listener nonblocking: {error}"))?;
    let address = listener
        .local_addr()
        .map_err(|error| format!("loopback listener address: {error}"))?;
    Ok((listener, address))
}

/// Hand each accepted connection to `admit` until `stopping` holds. An idle
/// listener waits `poll` before it rechecks.
///
/// # Errors
///
/// The listener's failure, which ends the loop.
pub fn accept_until(
    listener: &TcpListener,
    poll: Duration,
    stopping: impl Fn() -> bool,
    mut admit: impl FnMut(TcpStream),
) -> io::Result<()> {
    while !stopping() {
        match listener.accept() {
            Ok((stream, _)) => admit(stream),
            Err(error) if error.kind() == ErrorKind::WouldBlock => std::thread::park_timeout(poll),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Whether a socket read only waited out its timeout.
pub fn read_timed_out(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Shut down both directions of every stream in `streams`. A stream the peer
/// already closed has nothing left to cut.
pub fn shut_down_all(streams: impl IntoIterator<Item = TcpStream>) {
    for stream in streams {
        drop(stream.shutdown(Shutdown::Both));
    }
}

/// Lock `mutex`; a panicked holder left nothing half-written.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Run `work` on a new thread and register it in `spawned`, so
/// [`join_within`] joins it.
pub fn spawn_tracked(spawned: &Spawned, work: impl FnOnce() + Send + 'static) {
    lock(spawned).push(std::thread::spawn(work));
}

/// How long a scripted peer's idle acceptor, blocked read, or joining stop
/// waits before it rechecks.
pub const PEER_POLL: Duration = Duration::from_millis(20);

/// The join bound the fallback stop of an unwinding row runs under.
pub const PEER_UNWINDING: Duration = Duration::from_secs(5);

/// How a peer's threads learn to stop, and how a row tells them.
pub trait PeerStop {
    /// Whether the peer was told to stop.
    fn stopping(&self) -> bool;
    /// Tell the peer to stop and cut its open connections.
    fn stop(&self);
}

/// How one peer's threads run and stop.
pub struct PeerPlan<C, Admit> {
    /// How long an idle acceptor or a joining stop waits before it rechecks.
    pub poll: Duration,
    /// What the join names the threads it counts.
    pub what: &'static str,
    /// The join bound an unwinding row's fallback stop runs under.
    pub unwinding: Duration,
    /// Reads and raises the peer's stop.
    pub control: C,
    /// Take one accepted connection; a thread that serves it goes through
    /// [`spawn_tracked`] on the list it is handed.
    pub admit: Admit,
}

/// The one owner of a peer's threads: its acceptor, and every thread the
/// acceptor spawned. [`Self::finish`] stops and joins them within a bound;
/// `Drop` is the fallback for an unwinding row.
pub struct PeerThreads<C: PeerStop> {
    spawned: Arc<Spawned>,
    acceptor: Option<JoinHandle<io::Result<()>>>,
    control: C,
    poll: Duration,
    what: &'static str,
    unwinding: Duration,
}

impl<C: PeerStop> PeerThreads<C> {
    /// Bind a loopback listener and accept on it under `plan` until the peer
    /// stops.
    ///
    /// # Errors
    ///
    /// When the listener cannot bind, turn nonblocking, or report its address.
    pub fn start<Admit>(plan: PeerPlan<C, Admit>) -> Result<(Self, SocketAddr), String>
    where
        C: Clone + Send + 'static,
        Admit: FnMut(TcpStream, &Spawned) + Send + 'static,
    {
        let (listener, address) = bind_loopback()?;
        let PeerPlan {
            poll,
            what,
            unwinding,
            control,
            mut admit,
        } = plan;
        let spawned = Arc::new(Mutex::new(Vec::new()));
        let acceptor = {
            let spawned = Arc::clone(&spawned);
            let watched = control.clone();
            std::thread::spawn(move || {
                accept_until(
                    &listener,
                    poll,
                    || watched.stopping(),
                    |stream| admit(stream, &spawned),
                )
            })
        };
        let threads = Self {
            spawned,
            acceptor: Some(acceptor),
            control,
            poll,
            what,
            unwinding,
        };
        Ok((threads, address))
    }

    /// Stop the peer and join its threads within `bound`.
    ///
    /// # Errors
    ///
    /// Counts the threads still running after the bound, and the ones that
    /// panicked, and names a failed listener.
    pub fn finish(mut self, bound: Duration) -> Result<(), String> {
        self.stop(bound)
    }

    /// Finish the peer after a row, within `bound`, keeping the row's own
    /// verdict first.
    ///
    /// # Errors
    ///
    /// The row's failure, then what [`Self::finish`] reports.
    pub fn finished(self, bound: Duration, verdict: Row) -> Row {
        verdict.and(self.finish(bound))
    }

    fn stop(&mut self, bound: Duration) -> Result<(), String> {
        self.control.stop();
        join_within(
            self.acceptor.take(),
            &self.spawned,
            bound,
            self.poll,
            self.what,
        )
    }
}

impl<C: PeerStop> Drop for PeerThreads<C> {
    fn drop(&mut self) {
        if self.acceptor.is_some() {
            drop(self.stop(self.unwinding));
        }
    }
}

/// Join `acceptor`, then every thread in `spawned`, within `bound`,
/// rechecking each `poll`. Call it after the peer was told to stop.
///
/// The acceptor is joined first, so a thread it registered after the stop is
/// joined too. The spawned list is drained before any wait on it, so no lock
/// is held while a thread finishes.
///
/// # Errors
///
/// Counts the `what` still running after the bound, and the ones that
/// panicked, and names the error a failed acceptor's listener returned.
pub fn join_within(
    acceptor: Option<JoinHandle<io::Result<()>>>,
    spawned: &Spawned,
    bound: Duration,
    poll: Duration,
    what: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + bound;
    let mut ends = Ends::default();
    let failed = acceptor
        .and_then(|acceptor| ends.settle(acceptor, deadline, poll))
        .and_then(Result::err);
    let handles = std::mem::take(&mut *lock(spawned));
    for handle in handles {
        ends.settle(handle, deadline, poll);
    }
    match (ends, failed) {
        (
            Ends {
                running: 0,
                panicked: 0,
            },
            None,
        ) => Ok(()),
        (Ends { running, panicked }, None) => Err(format!(
            "{running} {what} outlived {bound:?}; {panicked} panicked"
        )),
        (Ends { running, panicked }, Some(error)) => Err(format!(
            "{running} {what} outlived {bound:?}; {panicked} panicked; the acceptor's listener failed: {error}"
        )),
    }
}

/// How the threads [`join_within`] waited on ended.
#[derive(Default)]
struct Ends {
    running: usize,
    panicked: usize,
}

impl Ends {
    /// Wait for `handle` until `deadline`, then count how it ended. Returns
    /// what a thread that returned answered.
    fn settle<T>(&mut self, handle: JoinHandle<T>, deadline: Instant, poll: Duration) -> Option<T> {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::park_timeout(poll);
        }
        match handle.is_finished().then(|| handle.join()) {
            None => {
                self.running += 1;
                None
            }
            Some(Err(_)) => {
                self.panicked += 1;
                None
            }
            Some(Ok(answer)) => Some(answer),
        }
    }
}

/// State a peer's threads and its row share, with a condvar every change
/// notifies.
pub struct SharedState<S> {
    state: Mutex<S>,
    changed: Condvar,
}

impl<S> SharedState<S> {
    /// Share `state`.
    pub const fn new(state: S) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
        }
    }

    /// Lock the state, through a poisoned lock.
    pub fn state(&self) -> MutexGuard<'_, S> {
        lock(&self.state)
    }

    /// Change the state, then wake every waiter.
    pub fn update<T>(&self, change: impl FnOnce(&mut S) -> T) -> T {
        let result = change(&mut self.state());
        self.changed.notify_all();
        result
    }

    /// Release `state` until the next change or `timeout`, then lock it again.
    pub fn wait_timeout<'a>(
        &self,
        state: MutexGuard<'a, S>,
        timeout: Duration,
    ) -> MutexGuard<'a, S> {
        self.changed
            .wait_timeout(state, timeout)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }

    /// Wait until `reached` holds for the part of the state `view` reads,
    /// within `bound`, and return a copy of that part.
    ///
    /// # Errors
    ///
    /// Names `what` and the last copy when the bound passes first.
    pub fn wait_for<V: Clone + Debug>(
        &self,
        what: &str,
        bound: Duration,
        view: impl Fn(&S) -> &V,
        reached: impl Fn(&V) -> bool,
    ) -> Result<V, String> {
        let deadline = Instant::now() + bound;
        let mut state = self.state();
        loop {
            let seen = view(&state);
            if reached(seen) {
                return Ok(seen.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{what} not observed within {bound:?}: {seen:?}"));
            }
            state = self.wait_timeout(state, left);
        }
    }
}

/// The state a scripted peer's threads and its row share: a log of what the
/// peer read, and the stop its threads watch.
pub trait PeerState {
    /// What the peer has read, in order.
    type Log: Clone + Debug;
    /// The log so far.
    fn log(&self) -> &Self::Log;
    /// Connections the peer accepted.
    fn accepted(&self) -> usize;
    /// Whether the peer was told to stop.
    fn stopping(&self) -> bool;
    /// Mark the peer stopping and cut its open connections.
    fn stop(&mut self);
}

/// Scripts and reads one peer. Holds no thread: dropping it stops nothing.
pub struct PeerControl<S> {
    shared: Arc<SharedState<S>>,
}

impl<S> Clone for PeerControl<S> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<S: PeerState> PeerControl<S> {
    /// Share `state` between a peer's threads and its row.
    pub fn new(state: S) -> Self {
        Self {
            shared: Arc::new(SharedState::new(state)),
        }
    }

    /// The state this control shares.
    pub fn shared(&self) -> &Arc<SharedState<S>> {
        &self.shared
    }

    /// A snapshot of what the peer has read.
    #[must_use]
    pub fn log(&self) -> S::Log {
        self.shared.state().log().clone()
    }

    /// Wait until `reached` holds for the log, within `bound`.
    ///
    /// # Errors
    ///
    /// Names `what` and the last log when the bound passes first.
    pub fn wait_for(
        &self,
        what: &str,
        bound: Duration,
        reached: impl Fn(&S::Log) -> bool,
    ) -> Result<S::Log, String> {
        self.shared.wait_for(what, bound, S::log, reached)
    }

    /// Fail the row unless the peer accepted no connection.
    ///
    /// # Errors
    ///
    /// The connections it accepted.
    pub fn expect_no_connection(&self) -> Row {
        expect_eq(
            "connections the peer accepted",
            self.shared.state().accepted(),
            0,
        )
    }
}

impl<S: PeerState> PeerStop for PeerControl<S> {
    fn stopping(&self) -> bool {
        self.shared.state().stopping()
    }

    fn stop(&self) {
        self.shared.update(S::stop);
    }
}
