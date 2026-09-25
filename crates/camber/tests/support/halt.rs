//! The one race every halted fixture runs between its halt and its work, the
//! one loopback listener built on it, and the report and close wait its
//! connections share.
//!
//! Mounted once per binary, beside the support files that use it, so a binary
//! that mounts both `ws_async` and `retry_upstream` compiles it once. Every
//! mount sits beside an `http` support module, which the listener's address
//! check reaches through `super`.

use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::net::SocketAddr;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinError, JoinHandle, JoinSet};

/// Run `work` unless the halt signal is raised first; `None` means it was.
///
/// The halt is polled first, so a connection a halted fixture still holds
/// gives up its phase instead of racing it. A dropped signal sender counts as a
/// halt: nothing is left that could stop the phase otherwise.
pub async fn unless_halted<F: Future>(
    halt: &mut watch::Receiver<bool>,
    work: F,
) -> Option<F::Output> {
    tokio::select! {
        biased;
        _ = halt.wait_for(|halted| *halted) => None,
        output = work => Some(output),
    }
}

/// Deliver one report to the row.
///
/// The row's receiver outlives every connection task: [`HaltableListener::finish`]
/// joins them all before it drains the channel. A send can only fail after a
/// row panicked and dropped its listener, and that row has already failed on
/// its own account; a second panic from a detached task would only bury it.
pub fn send_report<E>(events: &mpsc::UnboundedSender<E>, report: E) {
    let _ = events.send(report);
}

/// Read and discard until the peer closes the transport.
///
/// End of stream and a gone-peer error are both the peer's close. Any other
/// transport fault is returned for the caller to report.
pub async fn until_peer_closed(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut discard = [0_u8; 256];
    loop {
        match stream.read(&mut discard).await {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error) if super::http::is_closed_connection_error(&error) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

/// A loopback listener that owns its accept loop and every connection it
/// accepted, until it is halted.
///
/// Each connection is served by one task and reports what it reached as an
/// `E`. A fault is reported the same way instead of panicking inside a task,
/// so a row's own failure is never hidden behind a fixture timeout.
pub struct HaltableListener<E> {
    addr: SocketAddr,
    events: mpsc::UnboundedReceiver<E>,
    halt: watch::Sender<bool>,
    accepts: JoinHandle<()>,
}

impl<E: Debug + Send + 'static> HaltableListener<E> {
    /// Bind a loopback listener and serve every connection it accepts.
    ///
    /// `serving` receives the report sender once, before the first accept,
    /// and returns the per-connection serve function. That function gets the
    /// connection, its accept index, a report sender, and the halt signal; it
    /// must give up whatever phase it reached once the signal is raised, so a
    /// connection no row finished cannot park the join. `failed` builds the
    /// report for a fault of the accept loop itself.
    pub async fn bind<M, S, F>(what: &str, serving: M, failed: fn(usize, Box<str>) -> E) -> Self
    where
        M: FnOnce(&mpsc::UnboundedSender<E>) -> S,
        S: FnMut(TcpStream, usize, mpsc::UnboundedSender<E>, watch::Receiver<bool>) -> F
            + Send
            + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{what}: binding failed: {error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{what}: the address is unreadable: {error}"));
        let (events_tx, events) = mpsc::unbounded_channel();
        let (halt, halted) = watch::channel(false);
        let serve = serving(&events_tx);
        let accepts = tokio::spawn(accept_until_halted(
            listener, serve, failed, events_tx, halted,
        ));
        Self {
            addr,
            events,
            halt,
            accepts,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The next report, unbounded. The caller owns the bound, because a
    /// paused-clock case cannot use a Tokio timeout for it.
    pub async fn recv(&mut self) -> Option<E> {
        self.events.recv().await
    }

    /// Stop accepting, close every held connection, join the accept loop under
    /// `bound`, prove the address is free again, and require that nothing was
    /// left unreported.
    ///
    /// `bound` returns `None` when the join outlived it. A report no row
    /// consumed is a connection no row expected — a second dial, a plaintext
    /// fallback, a fixture fault — so it fails here rather than being dropped
    /// with the channel.
    pub async fn finish<B, J>(self, context: &str, bound: B)
    where
        B: FnOnce(JoinHandle<()>) -> J,
        J: Future<Output = Option<Result<(), JoinError>>>,
    {
        let Self {
            addr,
            mut events,
            halt,
            accepts,
        } = self;
        halt.send_replace(true);
        match bound(accepts).await {
            Some(Ok(())) => {}
            Some(Err(error)) => panic!("{context}: the listener did not join: {error}"),
            None => panic!("{context}: the listener did not join within its bound"),
        }
        super::http::assert_address_reused(addr, context).await;
        let mut unconsumed = Vec::new();
        while let Ok(event) = events.try_recv() {
            unconsumed.push(event);
        }
        assert!(
            unconsumed.is_empty(),
            "{context}: the listener reported what no row expected: {unconsumed:?}"
        );
    }
}

async fn accept_until_halted<E, S, F>(
    listener: TcpListener,
    mut serve: S,
    failed: fn(usize, Box<str>) -> E,
    events: mpsc::UnboundedSender<E>,
    mut halt: watch::Receiver<bool>,
) where
    S: FnMut(TcpStream, usize, mpsc::UnboundedSender<E>, watch::Receiver<bool>) -> F,
    F: Future<Output = ()> + Send + 'static,
{
    let mut connections = JoinSet::new();
    let mut accepted_as = HashMap::new();
    let mut index = 0;
    while let Some(accepted) = unless_halted(&mut halt, listener.accept()).await {
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(error) => {
                send_report(
                    &events,
                    failed(index, format!("accept failed: {error}").into()),
                );
                break;
            }
        };
        let task = connections.spawn(serve(stream, index, events.clone(), halt.clone()));
        accepted_as.insert(task.id(), index);
        index += 1;
    }
    drop(listener);
    while let Some(joined) = connections.join_next().await {
        if let Err(error) = joined {
            // The report names the connection that faulted, not how many were
            // accepted: every task id was recorded at its spawn.
            let faulted = accepted_as.get(&error.id()).copied().unwrap_or(index);
            send_report(
                &events,
                failed(
                    faulted,
                    format!("a connection task did not join: {error}").into(),
                ),
            );
        }
    }
}
