//! The one set-once response commitment an admitted operation has.
//!
//! Every pre-commit producer — the route handler behind dispatch, the
//! middleware gate, the router's own terminals, the framework's rejection
//! mapper, the streaming proxy's upstream head, the static-file worker, the SSE
//! and WebSocket handoffs, tonic's head, and Camber's internal routes — reaches
//! this one cell before it can answer the peer. The first owner that commits
//! owns the response; every later attempt is told what was already committed and
//! does only its own cleanup.
//!
//! What replaced a declared rank: there is no global order over the causes an
//! operation can end on, and no scheduling turn decides between two of them.
//! Commit order does, and commit order is what a public command, an owner's own
//! read, or a protocol acknowledgement establishes. Two facts with no such edge
//! between them may commit in either order, and both results are correct.

use super::completion::CompletionAccount;
use super::mock::LifecycleScript;
use super::operation::{InboundTerminal, OperationId};
use std::sync::{Arc, Mutex};

/// Which production owner produced an operation's committed response head.
///
/// Closed, and exhaustive over the producer table: an owner that answers a peer
/// names itself here, so a completion record states provenance rather than
/// inferring it from a status. Absence is its own fact — an operation that ended
/// before any head committed has no origin at all, which the commitment records
/// as a cause rather than an origin.
///
/// Every variant but [`Self::Protocol`] is written by a pre-commit producer that
/// reaches the commitment. `Protocol` is not a pre-commit producer at all, and
/// this cell never holds it; see the variant for why nothing else holds it
/// either.
///
/// A test seam, not API. Hidden from the documentation and outside the semver
/// promise this crate makes, on the same footing as
/// [`OperationStage`](super::operation::OperationStage).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResponseOrigin {
    /// An ordinary route handler's response.
    Application,
    /// A middleware chain that short-circuited its route.
    Middleware,
    /// The router's own not-found, method, or host terminal.
    Router,
    /// A typed framework rejection, after its mapper ran.
    Framework,
    /// A buffered or streaming proxy's upstream response head.
    Upstream,
    /// A static-file response.
    StaticFile,
    /// A server-sent-events handoff.
    ServerSentEvents,
    /// A tonic response handoff.
    #[cfg(feature = "grpc")]
    Grpc,
    /// A successful `101 Switching Protocols` handoff.
    #[cfg(feature = "ws")]
    WebSocket,
    /// A profiling, health, or other Camber-internal route.
    Internal,
    /// An accepted operation Hyper answered with no Camber producer.
    ///
    /// Never written here, and no served request reaches the state that
    /// publishes it either. Hyper answers on its own only below Camber's
    /// admitted-operation constructor, and such a request is recorded nowhere;
    /// past that point every response an operation gets is staged by the Camber
    /// exit that produced it, and every one of those exits names its producer.
    /// So the variant is unreachable by construction, and its proof is that no
    /// drive publishes it.
    ///
    /// Kept, and declared here rather than beside the finalizer, because the
    /// spec's producer table names this case and one spelling of it belongs
    /// where a reader looks for the closed set. The finalizer states it, and
    /// [`CompletionAccount::published_origin`](super::completion) is where the
    /// mapping lives if a future protocol owner ever makes the state real.
    Protocol,
}

impl ResponseOrigin {
    /// Every producer a completed operation can name.
    ///
    /// Named exhaustively rather than derived from declaration order, so a
    /// variant moved for readability cannot silently change what a live service
    /// publishes as its vocabulary.
    const ALL: [Self; ORIGIN_COUNT] = [
        Self::Application,
        Self::Middleware,
        Self::Router,
        Self::Framework,
        Self::Upstream,
        Self::StaticFile,
        Self::ServerSentEvents,
        #[cfg(feature = "grpc")]
        Self::Grpc,
        #[cfg(feature = "ws")]
        Self::WebSocket,
        Self::Internal,
        Self::Protocol,
    ];

    /// Every name an origin label may carry, including stated absence.
    ///
    /// Read off the declared list rather than transcribed beside it, so the
    /// vocabulary an operator is published and the producers a live request can
    /// name are one list.
    pub(super) fn vocabulary() -> Box<[&'static str]> {
        super::completion::optional_vocabulary(Self::ALL.map(Self::label))
    }

    /// The bounded name this producer is reported and counted under.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Middleware => "middleware",
            Self::Router => "router",
            Self::Framework => "framework",
            Self::Upstream => "upstream",
            Self::StaticFile => "static-file",
            Self::ServerSentEvents => "sse",
            #[cfg(feature = "grpc")]
            Self::Grpc => "grpc",
            #[cfg(feature = "ws")]
            Self::WebSocket => "websocket",
            Self::Internal => "internal",
            Self::Protocol => "protocol",
        }
    }
}

/// How many producers this build compiles in.
///
/// The two protocol handoffs are feature-gated, so the declared list is a
/// different width per build and the count has to be derived the same way.
const ORIGIN_COUNT: usize = 9 + cfg!(feature = "grpc") as usize + cfg!(feature = "ws") as usize;

/// What one operation's response commitment settled on.
///
/// Two arms because a peer is owed different things by each. A committed head
/// has a producer and a status the peer may already hold; a committed cause has
/// neither, and what the peer is owed is whatever the shared failure table maps
/// that cause to — or nothing at all, for the causes it maps silently.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseCommit {
    /// A pre-commit producer committed a response head.
    Head(ResponseOrigin),
    /// A cause ended this operation before any head was committed.
    Cause(InboundTerminal),
}

/// One admitted operation's shared, set-once response commitment.
///
/// Concurrent producer owners share one `Arc` of this: the identity is
/// immutable and readable without the lock, and the short critical section
/// covers only the cell that decides which of them owns the answer. Nothing
/// awaits while holding it, so a producer's commit attempt cannot be parked
/// behind another producer's work.
pub(super) struct OperationCommitment {
    id: OperationId,
    /// The first fact committed, once one has been.
    ///
    /// A `Mutex` and not an atomic: the value is two words wide and the claim is
    /// that the read-and-set is one step. A compare-exchange over a packed
    /// integer would state the same thing in a shape no owner here could read
    /// back without decoding it.
    committed: Mutex<Option<ResponseCommit>>,
    /// The listener's observer, while one is registered.
    ///
    /// Held here rather than passed per attempt, because every producer that
    /// reaches this cell must be counted alike: an owner that took the answer
    /// and an owner that arrived late are the two facts a set-once claim is
    /// made of, and a caller-supplied observer would let one of them go
    /// unrecorded.
    observer: Option<Arc<LifecycleScript>>,
    /// The completion account the producer that takes this cell names itself in.
    ///
    /// Held here rather than written by each producer, because the origin is a
    /// fact about the commitment: the owner that took the cell is the one the
    /// peer heard from, and an owner that arrived late would otherwise be able
    /// to name itself as the producer of an answer it never gave.
    account: Arc<CompletionAccount>,
}

impl OperationCommitment {
    /// The one commitment an admitted head mints.
    pub(super) fn minted(
        id: OperationId,
        observer: Option<Arc<LifecycleScript>>,
        account: Arc<CompletionAccount>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            committed: Mutex::new(None),
            observer,
            account,
        })
    }

    /// Record one produced response head, whether or not it takes the cell.
    ///
    /// The head producers do not branch on the answer: an owner that finds the
    /// cell taken has already been answered by whoever took it, and its own
    /// response goes to a peer that is either gone or already served. What the
    /// attempt still owes is to be counted, which is what makes the set-once
    /// claim provable rather than asserted.
    pub(super) fn record_head(&self, origin: ResponseOrigin) {
        // Matched rather than discarded, on the one thing the two outcomes do
        // ask differently: the owner that took the cell answered this peer and
        // names itself in the record, and the owner that found it taken did not.
        match self.commit(ResponseCommit::Head(origin)) {
            Ok(()) => self.account.record_origin(origin),
            Err(_committed) => {}
        }
    }

    /// Commit one pre-commit cause, or be told what already owns the answer.
    ///
    /// The cause that takes the cell tells the account this operation ended
    /// before any producer reached it. That absence is a fact of its own: a
    /// record with a status and no origin is an operation Camber answered from
    /// its own cause table, and it is exactly what must not be mistaken for a
    /// head Hyper wrote with no Camber owner behind it.
    pub(super) fn commit_cause(&self, terminal: InboundTerminal) -> Result<(), ResponseCommit> {
        let settled = self.commit(ResponseCommit::Cause(terminal));
        match settled {
            Ok(()) => self.account.record_uncommitted_head(),
            Err(_committed) => {}
        }
        settled
    }

    /// Take the cell, or answer with what is already in it.
    ///
    /// The whole set-once rule, stated once. A post-commit mapping is not a
    /// second commitment and not an error to report upward: it is an owner
    /// arriving late, which is told what won so it can clean up against the
    /// right answer.
    fn commit(&self, attempt: ResponseCommit) -> Result<(), ResponseCommit> {
        let settled = {
            let mut held = self.held();
            match *held {
                Some(committed) => Err(committed),
                None => {
                    *held = Some(attempt);
                    Ok(())
                }
            }
        };
        LifecycleScript::observe_commitment(self.observer.as_deref(), self.id, attempt, &settled);
        settled
    }

    /// Read the cell past a lock a panicking producer poisoned.
    ///
    /// A producer that panicked between taking the lock and writing left the
    /// cell exactly as it found it, so the commitment is still sound. Refusing
    /// to read it would turn one failed request into every later owner of this
    /// operation panicking on the lock instead.
    fn held(&self) -> std::sync::MutexGuard<'_, Option<ResponseCommit>> {
        self.committed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Name the producer of a head refused before any operation was minted.
///
/// The one origin written with no cell behind it, and it is written from here so
/// the dimension keeps one owning module: nothing outside this file reaches
/// `CompletionAccount::record_origin`, exactly as every other fact the account
/// holds is reached from the single owner that can state it.
///
/// There is no cell to arbitrate because there is no operation. A head refused
/// where its route is classified answers and returns before one is minted, so a
/// request either arrives here and never mints a commitment, or mints one and
/// never arrives here. The two are exclusive, and the mapper that answers is the
/// only producer the peer heard from.
pub(super) fn record_prehead_origin(account: &CompletionAccount, origin: ResponseOrigin) {
    account.record_origin(origin);
}
