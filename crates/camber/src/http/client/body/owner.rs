//! The owner of one streamed body: the engine that reads its source.
//!
//! The engine runs inline in the task that owns the source. It starts no task
//! of its own. It reads the source only under an accepted demand, and it reads
//! one nonempty chunk for each demand. It holds no chunk between demands.
//!
//! The owner ends on one terminal fact. It commits the fact, drops the source,
//! publishes the operation's terminal record when it carries one, and then
//! acknowledges. An owner that is dropped unfinished follows the same order,
//! because the order is the order of its fields.

use super::super::super::body_admission::checked_body_frame_total;
use super::super::completion::Completion;
use super::super::error::{ClientError, ClientTimeout};
use super::super::lifetime::{ATTEMPT_EXPIRED, Lifetime, LifetimeEnd, OPERATION_EXPIRED};
use super::state::{BodyCounts, BodyTerminal, Shared};
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::{Body, Frame};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The bounds one body is read under.
pub(in crate::http::client) struct BodyBudget {
    /// The most payload bytes the body may deliver.
    pub(in crate::http::client) limit: usize,
    /// The running total the count starts at.
    pub(in crate::http::client) counted: usize,
    /// How long one accepted demand may wait for a nonempty chunk.
    pub(in crate::http::client) idle: Duration,
    /// The operation's own lifetime, fixed at its first poll.
    pub(in crate::http::client) lifetime: Lifetime,
    /// When the final attempt expires, as its send armed it.
    pub(in crate::http::client) attempt: Option<Instant>,
}

/// The acknowledgement an owner owes its handle.
///
/// Its drop fixes the terminal fact, publishes the operation's record of it,
/// and then marks the fact settled. The owner declares it after the source,
/// so the source is always released first. A caller that reads the settled
/// fact therefore finds the record already published.
struct Settlement {
    shared: Arc<Shared>,
    /// The terminal record of the operation this body ends, or `None` for a
    /// body no operation records.
    record: Option<Completion>,
}

impl Drop for Settlement {
    fn drop(&mut self) {
        let terminal = self.shared.conclude();
        if let Some(record) = self.record.take() {
            record.settle(terminal.recorded());
        }
        self.shared.settle(terminal);
    }
}

/// The caller's side of one body: the sole access to its demands.
///
/// Its drop abandons the body. An access that is lost on its way to the
/// caller therefore releases the source, like one the caller dropped.
pub(in crate::http::client) struct BodyAccess(Arc<Shared>);

impl BodyAccess {
    /// A read-only view of the body's counts.
    pub(in crate::http::client) fn watch(&self) -> BodyWatch {
        BodyWatch(Arc::clone(&self.0))
    }

    pub(super) fn shared(&self) -> &Shared {
        &self.0
    }
}

impl Drop for BodyAccess {
    fn drop(&mut self) {
        self.0.abandon();
    }
}

/// A read-only view of one body's counts.
pub(in crate::http::client) struct BodyWatch(Arc<Shared>);

impl BodyWatch {
    pub(super) fn counts(&self) -> BodyCounts {
        self.0.counts()
    }
}

/// The owner of one body source.
///
/// The fields are dropped in the order they are declared: the source, then
/// the settlement. An aborted owner therefore releases its source before its
/// handle reads a terminal fact.
pub(in crate::http::client) struct BodyOwner<S> {
    source: S,
    budget: BodyBudget,
    settlement: Settlement,
}

/// What one read of the source returned.
enum Read {
    /// One nonempty data frame.
    Chunk(Bytes),
    /// The clean end of the body.
    End,
    /// A failed read. A truncated body is one.
    Failed,
}

/// The payload of `frame`, or `None` for an empty data frame or trailers.
fn payload(frame: Frame<Bytes>) -> Option<Bytes> {
    frame.into_data().ok().filter(|data| !data.is_empty())
}

/// Read the source up to its next nonempty chunk, its end, or its failure.
///
/// Empty data frames and trailers are framing. They are read and dropped, and
/// the caller's timers keep running across them.
async fn read_chunk<S>(source: &mut S) -> Read
where
    S: Body<Data = Bytes> + Unpin,
{
    loop {
        let frame = match source.frame().await {
            None => return Read::End,
            Some(Err(_)) => return Read::Failed,
            Some(Ok(frame)) => frame,
        };
        if let Some(chunk) = payload(frame) {
            return Read::Chunk(chunk);
        }
    }
}

/// The fact a lifetime ended the body on.
const fn lifetime_end(end: LifetimeEnd) -> BodyTerminal {
    match end {
        LifetimeEnd::Expired => BodyTerminal::Failed(OPERATION_EXPIRED),
        LifetimeEnd::Stopped => BodyTerminal::Stopped,
    }
}

/// The output of a wait under the carried expiries, or the fact that ended it.
fn carried<T>(waited: Result<Option<T>, LifetimeEnd>) -> Result<T, BodyTerminal> {
    match waited {
        Ok(Some(output)) => Ok(output),
        Ok(None) => Err(BodyTerminal::Failed(ATTEMPT_EXPIRED)),
        Err(cause) => Err(lifetime_end(cause)),
    }
}

impl<S> BodyOwner<S>
where
    S: Body<Data = Bytes> + Unpin,
{
    /// Make the owner of `source` and the caller's side of its state.
    ///
    /// `record` is the terminal record of the operation this body ends. The
    /// owner settles it on the body's own fact, after the source is released.
    ///
    /// The settlement exists from here. An owner that is dropped before it
    /// runs still releases its source, publishes, and acknowledges.
    pub(in crate::http::client) fn new(
        source: S,
        budget: BodyBudget,
        record: Option<Completion>,
    ) -> (Self, BodyAccess) {
        let shared = Arc::new(Shared::new());
        let access = BodyAccess(Arc::clone(&shared));
        let owner = Self {
            source,
            budget,
            settlement: Settlement { shared, record },
        };
        (owner, access)
    }

    /// Serve demands until the body ends, then commit the fact it ended on.
    ///
    /// The owner is dropped at the return. That drop releases the source and
    /// then acknowledges.
    pub(in crate::http::client) async fn run(mut self) {
        let terminal = self.ended().await;
        self.settlement.shared.commit(terminal);
    }

    /// Serve demands until one of them ends the body.
    async fn ended(&mut self) -> BodyTerminal {
        loop {
            if let Err(terminal) = self.serve().await {
                return terminal;
            }
        }
    }

    /// Wait for one demand, and answer it with one admitted chunk.
    ///
    /// Both waits run under the carried operation lifetime and attempt
    /// expiry, so each of them can end a body that has no demand.
    async fn serve(&mut self) -> Result<(), BodyTerminal> {
        let shared = &*self.settlement.shared;
        let budget = &self.budget;
        let demand = std::future::poll_fn(|context| shared.poll_demand(context));
        carried(budget.lifetime.within_attempt(budget.attempt, demand).await)??;
        let answer = answered(shared, &mut self.source, budget.idle);
        let chunk = carried(budget.lifetime.within_attempt(budget.attempt, answer).await)??;
        self.admit(chunk)
    }

    /// Count `chunk` and hand it to the caller, or refuse it whole.
    ///
    /// The count is checked before the chunk is delivered. A crossing chunk
    /// and an overflowing total are the same refusal, and the chunk is
    /// dropped here.
    fn admit(&mut self, chunk: Bytes) -> Result<(), BodyTerminal> {
        let limit = self.budget.limit;
        self.budget.counted = checked_body_frame_total(self.budget.counted, chunk.len(), limit)
            .ok_or(BodyTerminal::Failed(ClientError::ResponseTooLarge {
                limit,
            }))?;
        self.settlement.shared.deliver(chunk)
    }
}

/// The category of a demand that outlived its idle interval.
const RESPONSE_IDLE: ClientError = ClientError::Timeout(ClientTimeout::ResponseIdle);

/// Read the one nonempty chunk an accepted demand asked for.
///
/// The idle interval is armed here, once, and no frame moves it. A fact the
/// caller commits ends the read. The select names no winner: that fact is
/// already committed, and no commit replaces it.
///
/// Every failed read is a transport failure. The source does not name the
/// category.
async fn answered<S>(shared: &Shared, source: &mut S, idle: Duration) -> Result<Bytes, BodyTerminal>
where
    S: Body<Data = Bytes> + Unpin,
{
    let withdrawn = std::future::poll_fn(|context| shared.poll_withdrawn(context));
    let read = tokio::select! {
        biased;
        terminal = withdrawn => return Err(terminal),
        read = tokio::time::timeout(idle, read_chunk(source)) => read,
    };
    match read {
        Ok(Read::Chunk(chunk)) => Ok(chunk),
        Ok(Read::End) => Err(BodyTerminal::End),
        Ok(Read::Failed) => Err(BodyTerminal::Failed(ClientError::Transport)),
        Err(_) => Err(BodyTerminal::Failed(RESPONSE_IDLE)),
    }
}
