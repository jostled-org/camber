//! A loopback TCP relay a row places between a client and a real service.
//!
//! The relay forwards both directions unchanged. A row can refuse and cut its
//! connections to simulate transport loss, and read every byte clients sent
//! through it. A byte recorded here has left the client's transport, so the
//! record is causal evidence that a request was submitted.
//!
//! The relay owns its listener and every pump thread. [`Relay::finish`] stops
//! and joins them within a bound; `Drop` is the fallback for an unwinding row.

use crate::scripted_peer::{
    PeerPlan, PeerStop, PeerThreads, Spawned, lock, shut_down_all, spawn_tracked,
};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

/// How long an idle acceptor or a joining stop waits before it rechecks.
const POLL: Duration = Duration::from_millis(10);

/// A loopback relay to one upstream address.
pub struct Relay {
    address: SocketAddr,
    control: RelayControl,
    threads: PeerThreads<RelayControl>,
}

/// Cuts, restores, and reads one relay. Holds no thread: dropping it stops
/// nothing.
#[derive(Clone)]
pub struct RelayControl {
    shared: Arc<Shared>,
}

/// What the relay's threads share.
struct Shared {
    refusing: AtomicBool,
    stopping: AtomicBool,
    /// Every client byte, in arrival order across connections.
    sent: watch::Sender<Vec<u8>>,
    open: Mutex<Vec<TcpStream>>,
    /// The bound an upstream connect and the stopping join run under.
    bound: Duration,
}

impl Relay {
    /// Bind a loopback listener and start relaying to `upstream`. `bound`
    /// limits each upstream connect and the join in [`Self::finish`].
    ///
    /// # Errors
    ///
    /// When the listener cannot bind or report its address.
    pub fn start(upstream: SocketAddr, bound: Duration) -> Result<Self, String> {
        let control = RelayControl {
            shared: Arc::new(Shared {
                refusing: AtomicBool::new(false),
                stopping: AtomicBool::new(false),
                sent: watch::Sender::new(Vec::new()),
                open: Mutex::new(Vec::new()),
                bound,
            }),
        };
        let accepting = control.clone();
        let (threads, address) = PeerThreads::start(PeerPlan {
            poll: POLL,
            what: "relay threads",
            unwinding: bound,
            control: control.clone(),
            admit: move |client: TcpStream, pumps: &Spawned| match accepting
                .shared
                .refusing
                .load(Ordering::SeqCst)
            {
                true => drop(client.shutdown(Shutdown::Both)),
                false => relay_pair(client, upstream, &accepting, pumps),
            },
        })
        .map_err(|error| format!("relay: {error}"))?;
        Ok(Self {
            address,
            control,
            threads,
        })
    }

    /// The loopback address clients connect to.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Cut, restore, and read this relay.
    #[must_use]
    pub fn control(&self) -> RelayControl {
        self.control.clone()
    }

    /// Stop the relay and join its threads within its bound.
    ///
    /// # Errors
    ///
    /// Names the threads still running after the bound.
    pub fn finish(self) -> Result<(), String> {
        self.threads.finish(self.control.shared.bound)
    }
}

impl RelayControl {
    /// Refuse new connections and cut the open ones, or accept again.
    pub fn set_refusing(&self, refusing: bool) {
        self.shared.refusing.store(refusing, Ordering::SeqCst);
        if refusing {
            self.cut();
        }
    }

    /// A view of every byte clients have sent through the relay.
    #[must_use]
    pub fn sent(&self) -> watch::Receiver<Vec<u8>> {
        self.shared.sent.subscribe()
    }

    /// Shut down every open stream. Call it after raising `refusing` or
    /// `stopping`: a pair registered after this drain sees the flag.
    fn cut(&self) {
        shut_down_all(lock(&self.shared.open).drain(..));
    }

    /// Register a pair's streams for [`Self::cut`], unless the relay is
    /// already refusing or stopping. The check runs under the same lock
    /// `cut` drains, so no pair escapes a cut that raced its registration.
    fn register(&self, streams: [TcpStream; 2]) -> bool {
        let mut open = lock(&self.shared.open);
        match self.shared.refusing.load(Ordering::SeqCst)
            || self.shared.stopping.load(Ordering::SeqCst)
        {
            true => {
                shut_down_all(streams);
                false
            }
            false => {
                open.extend(streams);
                true
            }
        }
    }
}

impl PeerStop for RelayControl {
    fn stopping(&self) -> bool {
        self.shared.stopping.load(Ordering::SeqCst)
    }

    fn stop(&self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.cut();
    }
}

/// Pump one client and its upstream connection both ways, recording what
/// the client sends.
fn relay_pair(client: TcpStream, upstream: SocketAddr, control: &RelayControl, pumps: &Spawned) {
    // An accepted socket can inherit the listener's nonblocking mode.
    let (Ok(()), Ok(server)) = (
        client.set_nonblocking(false),
        TcpStream::connect_timeout(&upstream, control.shared.bound),
    ) else {
        drop(client.shutdown(Shutdown::Both));
        return;
    };
    // A pair whose streams cannot all be cloned could not be cut, so it is
    // not relayed at all.
    let (Ok(client_read), Ok(server_read), Ok(client_open), Ok(server_open)) = (
        client.try_clone(),
        server.try_clone(),
        client.try_clone(),
        server.try_clone(),
    ) else {
        drop(client.shutdown(Shutdown::Both));
        drop(server.shutdown(Shutdown::Both));
        return;
    };
    if !control.register([client_open, server_open]) {
        return;
    }
    let recording = Arc::clone(&control.shared);
    spawn_tracked(pumps, move || {
        pump(client_read, server, |bytes| {
            recording
                .sent
                .send_modify(|sent| sent.extend_from_slice(bytes));
        });
    });
    spawn_tracked(pumps, move || {
        pump(server_read, client, |_| {});
    });
}

/// Copy `from` into `to` until either side ends, then end both. `record`
/// sees each chunk before it is forwarded.
fn pump(mut from: TcpStream, mut to: TcpStream, record: impl Fn(&[u8])) {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let chunk = match from.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => &buffer[..read],
        };
        record(chunk);
        if to.write_all(chunk).is_err() {
            break;
        }
    }
    drop(from.shutdown(Shutdown::Both));
    drop(to.shutdown(Shutdown::Both));
}
