//! The controlled lookup edge of one client, for tests.
//!
//! No local test can change what the system resolver answers. An edge attached
//! to a client supplies the addresses a lookup returned, and counts the
//! lookups. It supplies no verdict: every address it returns is read by the
//! same visit bound, the same policy decision, and the same pinned transport
//! as an address the system resolver returned.

use crate::runtime_state::recover_poisoned;
use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{Notify, oneshot};

/// What the next unanswered lookup is given.
enum Scripted {
    /// These addresses. None is a lookup that found no address.
    Answer(Box<[IpAddr]>),
    /// A failure.
    Fail,
    /// A wait, until the edge is released.
    Hold,
}

/// The scripted answers, and the lookup that waits for a release.
#[derive(Default)]
struct Script {
    queued: VecDeque<Scripted>,
    /// The sender a parked lookup waits on.
    parked: Option<oneshot::Sender<Box<[IpAddr]>>>,
}

/// What one edge and every client it is attached to share.
#[derive(Default)]
struct Shared {
    script: Mutex<Script>,
    lookups: AtomicUsize,
    /// Notified when a lookup parks. The permit is kept, so a wait that starts
    /// after the lookup parked still returns.
    held: Notify,
}

/// The lookup answers of one restricted client, chosen by a test.
///
/// Attach it with `client_dns_edge`. A lookup with nothing queued fails, so an
/// operation that resolves twice is visible in the count and in the result.
///
/// Semver-unsupported. An edge supplies addresses and reads a count. It cannot
/// admit a destination, refuse one, or make a connection.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct ClientDnsEdge {
    shared: Arc<Shared>,
}

impl ClientDnsEdge {
    /// An edge with no answers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue the answer of the next unanswered lookup.
    pub fn answer(&self, addresses: &[IpAddr]) {
        self.queue(Scripted::Answer(addresses.into()));
    }

    /// Queue a lookup that fails.
    pub fn fail(&self) {
        self.queue(Scripted::Fail);
    }

    /// Queue a lookup that waits for [`release`](Self::release).
    pub fn hold(&self) {
        self.queue(Scripted::Hold);
    }

    /// Wait until a lookup is parked at a queued hold.
    pub async fn held(&self) {
        self.shared.held.notified().await;
    }

    /// Answer the parked lookup with `addresses`.
    ///
    /// A release changes nothing when no lookup is parked, or when the
    /// operation that parked one has ended.
    pub fn release(&self, addresses: &[IpAddr]) {
        if let Some(parked) = self.script().parked.take() {
            // The receiver is gone when the operation ended.
            let _ = parked.send(addresses.into());
        }
    }

    /// How many lookups the client started.
    #[must_use]
    pub fn lookups(&self) -> usize {
        self.shared.lookups.load(Ordering::SeqCst)
    }

    /// Run one lookup: its scripted answer, or `None` for a failure.
    pub(super) async fn lookup(&self) -> Option<Box<[IpAddr]>> {
        self.shared.lookups.fetch_add(1, Ordering::SeqCst);
        let next = self.script().queued.pop_front();
        match next {
            Some(Scripted::Answer(addresses)) => Some(addresses),
            Some(Scripted::Hold) => self.parked().await,
            Some(Scripted::Fail) | None => None,
        }
    }

    /// Park this lookup until the edge is released.
    async fn parked(&self) -> Option<Box<[IpAddr]>> {
        let (release, released) = oneshot::channel();
        self.script().parked = Some(release);
        self.shared.held.notify_one();
        released.await.ok()
    }

    fn queue(&self, scripted: Scripted) {
        self.script().queued.push_back(scripted);
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        recover_poisoned(self.shared.script.lock())
    }
}

impl std::fmt::Debug for ClientDnsEdge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientDnsEdge")
            .field("lookups", &self.lookups())
            .finish()
    }
}
