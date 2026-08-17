//! One streaming multipart session: its driver, its command protocol, and the
//! two handles an application reads it through.
//!
//! The driver owns everything: the incoming body, the admitted byte budget, the
//! admission permit, the parser, and the one transport frame being consumed. The
//! application owns only the right to ask for the next thing. That split is what
//! makes a handler that stops reading stop the transport, and what lets the route
//! finalizer release every framework resource even when the handler moved its
//! access handle somewhere else.
//!
//! Exactly one command is in flight at a time. The driver polls ingress only
//! while servicing it, publishes at most one reply, and waits for that reply to
//! be acknowledged before admitting another. Losing an accepted operation before
//! its acknowledgment abandons the session; it cannot silently skip bytes into a
//! later success.

use super::grammar::malformed;
use super::limits::MultipartLimits;
use super::parser::{FieldMetadata, IncrementalParser, ParserEvent};
use crate::RuntimeError;
use crate::http::body_admission::{AdmittedBody, BodyBudget, BodyPermit};
use crate::http::mock::{LifecycleCheckpoint, LifecycleScript};
use crate::http::operation::InboundTerminal;
use crate::http::transfer::{IncomingSource, Transfer, TransferFailure, TransferOwner};
use bytes::Bytes;
use std::fmt::Display;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::Notify;

/// An operation issued against a session that no longer admits one.
const SESSION_CLOSED: &str = "multipart session is closed";

/// A reply that does not answer the command it was published for.
const UNEXPECTED_REPLY: &str = "multipart session produced an unexpected reply";

/// A session that ended before its closing delimiter reached end of body.
const INCOMPLETE_BODY: &str = "multipart body was not read through its closing delimiter";

/// What one session publishes its counters and checkpoints through.
///
/// `None` is the ordinary served request: no controller watches its listener,
/// so every counter call is a branch that does nothing and every checkpoint runs
/// straight through.
pub(in crate::http) type SessionObserver = Option<Arc<LifecycleScript>>;

/// Report one session event, and do nothing when nothing is watching.
///
/// Every counter below reaches its field through here, so "inert with no
/// controller registered" is decided once rather than restated per call site.
fn observe(observer: &SessionObserver, record: impl FnOnce(&SessionMetrics)) {
    match observer {
        Some(script) => record(script.multipart()),
        None => {}
    }
}

/// How one multipart session ended.
///
/// Every variant is terminal, and a running session is not one of them: the
/// driver holds its own state as an absent option until it settles, so no
/// pre-terminal state exists in what it returns and no reader has to re-derive
/// what a running session would have meant.
#[derive(Debug)]
pub(in crate::http) enum MultipartTerminal {
    /// Terminal framing was consumed through end of body.
    Clean,
    /// A parser, byte-limit, or transport failure ended the session.
    ParserFailure(MultipartFailure, Box<str>),
    /// An incomplete field, a canceled operation, or revocation ended it.
    Abandoned,
    /// The selected upload owner fixed one inbound or transfer terminal.
    ///
    /// Held apart from a parser failure because the answer is not this session's
    /// to shape: the declared precedence already named the cause, and the route
    /// answers it with the one disposition that cause carries.
    Ended(TransferFailure),
}

impl MultipartTerminal {
    /// What this terminal state lets the route commit.
    pub(in crate::http) fn completion(self) -> MultipartCompletion {
        match self {
            Self::Clean => MultipartCompletion::Complete,
            Self::ParserFailure(failure, diagnostic) => {
                MultipartCompletion::Failed(failure.restate(diagnostic))
            }
            Self::Abandoned => MultipartCompletion::Incomplete(malformed(INCOMPLETE_BODY)),
            Self::Ended(failure) => MultipartCompletion::Ended(failure),
        }
    }
}

/// What one finished session establishes about the answer.
///
/// Held apart from [`MultipartTerminal`] because the two say different things:
/// the terminal is how the session ended, and this is what the route may still
/// commit once its handler has also ended.
pub(in crate::http) enum MultipartCompletion {
    /// Terminal framing was consumed through end of body.
    Complete,
    /// A recorded parser, byte-limit, or transport failure ended the session.
    /// It outranks whatever the handler said.
    Failed(RuntimeError),
    /// The session ended with payload unread. A handler that reported its own
    /// failure keeps it; one that claimed success is answered with this.
    Incomplete(RuntimeError),
    /// The selected upload owner fixed one inbound or transfer terminal. It owes
    /// the peer exactly what the declared precedence gives that cause, and
    /// nothing the handler said can change it.
    Ended(TransferFailure),
}

/// Which kind of failure ended a session.
///
/// Recorded when the failure happens, so the finalizer maps its provenance
/// rather than telling three different faults apart by their text later.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum MultipartFailure {
    /// A total or per-field byte crossing.
    ByteLimit,
    /// An incoming transport read failed.
    Unreadable,
    /// The grammar, a structural bound, or the framing was violated.
    Structural,
}

impl MultipartFailure {
    /// Split one failure into the provenance and the diagnostic it carries.
    ///
    /// The diagnostic is moved out of the error, never rendered from it: the
    /// `Display` text already opens with the variant's own prefix, so recording
    /// that render and restating it later showed operators the prefix twice.
    /// Moving the payload makes the restatement reproduce the error exactly.
    fn split(error: RuntimeError) -> (Self, Box<str>) {
        match error {
            RuntimeError::RequestBodyLimit(diagnostic) => (Self::ByteLimit, diagnostic),
            RuntimeError::RequestBodyUnreadable(diagnostic) => (Self::Unreadable, diagnostic),
            RuntimeError::Multipart(diagnostic) => (Self::Structural, diagnostic),
            other => (Self::Structural, other.to_string().into()),
        }
    }

    /// Restate one recorded failure as the error its provenance names.
    ///
    /// The category was fixed where the failure happened, so the finalizer maps
    /// the same provenance the operation returned rather than telling three
    /// faults apart by their text after the fact.
    fn restate(self, diagnostic: Box<str>) -> RuntimeError {
        match self {
            Self::ByteLimit => RuntimeError::RequestBodyLimit(diagnostic),
            Self::Unreadable => RuntimeError::RequestBodyUnreadable(diagnostic),
            Self::Structural => RuntimeError::Multipart(diagnostic),
        }
    }
}

/// What the application asked the driver for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandKind {
    NextField,
    NextChunk,
    Discard,
}

/// The one bounded answer a command receives.
enum SessionReply {
    Field(FieldMetadata),
    Chunk(Bytes),
    FieldEnd,
    End,
    Failed(RuntimeError),
}

/// The read-only counters one session publishes while it runs.
///
/// Every value is written by the production decision it names and read by
/// whoever registered the counters. Nothing here chooses a limit, selects a
/// terminal state, or owns a resource.
#[derive(Default)]
pub(in crate::http) struct SessionMetrics {
    body_frames_polled: AtomicUsize,
    commands_accepted: AtomicUsize,
    parser_retained_bytes: AtomicUsize,
    parser_peak_bytes: AtomicUsize,
    reply_retained_bytes: AtomicUsize,
    reply_peak_bytes: AtomicUsize,
    active_metadata_peak_bytes: AtomicUsize,
    replies_published: AtomicUsize,
    source_frames_released: AtomicUsize,
    permit_owners_dropped: AtomicUsize,
    revocations: AtomicUsize,
    drivers_terminated: AtomicUsize,
    /// Woken when a reply is published, so an observer can wait for one rather
    /// than poll for it.
    published: Notify,
}

impl SessionMetrics {
    /// Count one published reply and wake whoever is waiting for it.
    fn count_reply(&self) {
        self.replies_published.fetch_add(1, Ordering::AcqRel);
        self.published.notify_waiters();
    }

    /// How many replies this session has published.
    pub(in crate::http) fn replies_published(&self) -> usize {
        self.replies_published.load(Ordering::Acquire)
    }

    /// Wait until this session has published at least `count` replies.
    pub(in crate::http) async fn wait_for_replies(&self, count: usize) {
        loop {
            let waiting = self.published.notified();
            tokio::pin!(waiting);
            waiting.as_mut().enable();
            if self.replies_published() >= count {
                return;
            }
            waiting.await;
        }
    }

    /// Count one polled data frame.
    fn count_frame(&self) {
        self.body_frames_polled.fetch_add(1, Ordering::AcqRel);
    }

    /// Count one accepted command.
    fn count_command(&self) {
        self.commands_accepted.fetch_add(1, Ordering::AcqRel);
    }

    /// Record what the parser is holding now, and the most it ever held.
    fn record_parser(&self, retained: usize, peak: usize) {
        self.parser_retained_bytes
            .store(retained, Ordering::Release);
        self.parser_peak_bytes.fetch_max(peak, Ordering::AcqRel);
    }

    /// Record what the one outstanding reply payload owns.
    fn record_reply(&self, bytes: usize) {
        self.reply_retained_bytes.store(bytes, Ordering::Release);
        self.reply_peak_bytes.fetch_max(bytes, Ordering::AcqRel);
    }

    /// Record one active field's retained metadata.
    fn record_active_metadata(&self, bytes: usize) {
        self.active_metadata_peak_bytes
            .fetch_max(bytes, Ordering::AcqRel);
    }

    /// How many payload frames this session polled.
    pub(in crate::http) fn body_frames_polled(&self) -> usize {
        self.body_frames_polled.load(Ordering::Acquire)
    }

    /// How many commands the driver accepted.
    pub(in crate::http) fn commands_accepted(&self) -> usize {
        self.commands_accepted.load(Ordering::Acquire)
    }

    /// What the parser budget is holding now.
    pub(in crate::http) fn parser_retained_bytes(&self) -> usize {
        self.parser_retained_bytes.load(Ordering::Acquire)
    }

    /// The most the parser budget ever held.
    pub(in crate::http) fn parser_peak_bytes(&self) -> usize {
        self.parser_peak_bytes.load(Ordering::Acquire)
    }

    /// What the one outstanding reply payload owns now.
    pub(in crate::http) fn reply_retained_bytes(&self) -> usize {
        self.reply_retained_bytes.load(Ordering::Acquire)
    }

    /// The largest in-flight reply payload this session published.
    pub(in crate::http) fn reply_peak_bytes(&self) -> usize {
        self.reply_peak_bytes.load(Ordering::Acquire)
    }

    /// The largest active field metadata payload this session retained.
    pub(in crate::http) fn active_metadata_peak_bytes(&self) -> usize {
        self.active_metadata_peak_bytes.load(Ordering::Acquire)
    }

    /// Count one spent source frame whose backing the driver released.
    fn count_source_released(&self) {
        self.source_frames_released.fetch_add(1, Ordering::AcqRel);
    }

    /// How many source frames released their backing.
    pub(in crate::http) fn source_frames_released(&self) -> usize {
        self.source_frames_released.load(Ordering::Acquire)
    }

    /// Count one admitted permit owner this session released.
    fn count_permit_owner_dropped(&self) {
        self.permit_owners_dropped.fetch_add(1, Ordering::AcqRel);
    }

    /// How many admitted permit owners this session's driver released.
    ///
    /// Written by the driver that holds the permit, so it counts this session's
    /// own admission and never another request class sharing the listener.
    pub(in crate::http) fn permit_owners_dropped(&self) -> usize {
        self.permit_owners_dropped.load(Ordering::Acquire)
    }

    /// Count one coordinator revocation.
    fn count_revocation(&self) {
        self.revocations.fetch_add(1, Ordering::AcqRel);
    }

    /// How many sessions had their command admission revoked.
    pub(in crate::http) fn revocations(&self) -> usize {
        self.revocations.load(Ordering::Acquire)
    }

    /// Count one driver that returned its terminal summary.
    fn count_termination(&self) {
        self.drivers_terminated.fetch_add(1, Ordering::AcqRel);
    }

    /// How many drivers returned their terminal summary.
    pub(in crate::http) fn drivers_terminated(&self) -> usize {
        self.drivers_terminated.load(Ordering::Acquire)
    }
}

/// Where the one in-flight command currently stands.
enum SlotPhase {
    /// No command exists.
    Idle,
    /// Issued by an operation, not yet accepted by the driver.
    Issued(CommandKind),
    /// Accepted by the driver, which may now run ingress.
    Accepted,
    /// Published by the driver, not yet acknowledged by its operation.
    Published(SessionReply),
    /// The operation was lost after acceptance.
    Lost,
}

/// What the driver's attempt to take a command decided.
enum Acceptance {
    Accepted(CommandKind),
    Pending,
    Closed,
}

struct SlotInner {
    /// Whether the route coordinator still admits commands.
    admitting: bool,
    /// Whether the access handle still exists.
    attached: bool,
    /// Whether the driver has returned.
    terminated: bool,
    /// Whether an incomplete field or a canceled operation poisoned the session.
    abandoned: bool,
    phase: SlotPhase,
    /// Bytes the one active field's metadata retains.
    active_metadata: usize,
}

impl SlotInner {
    /// Whether this session still admits ingress work.
    fn open(&self) -> bool {
        self.admitting && self.attached && !self.abandoned
    }

    /// Take one issued command, or say why the driver should stop waiting.
    fn try_accept(&mut self) -> Acceptance {
        match (&self.phase, self.open()) {
            (SlotPhase::Issued(kind), true) => {
                let kind = *kind;
                self.phase = SlotPhase::Accepted;
                Acceptance::Accepted(kind)
            }
            (SlotPhase::Lost, _) | (_, false) => Acceptance::Closed,
            (_, true) => Acceptance::Pending,
        }
    }

    /// Publish one reply against an accepted command.
    fn publish(&mut self, reply: SessionReply) -> bool {
        match self.phase {
            SlotPhase::Accepted => {
                self.phase = SlotPhase::Published(reply);
                true
            }
            _ => false,
        }
    }

    /// Whether the published reply has been acknowledged, and whether the
    /// acknowledgment arrived at all.
    fn acknowledged(&self) -> Option<bool> {
        match (&self.phase, self.open()) {
            (SlotPhase::Idle | SlotPhase::Issued(_), _) => Some(true),
            (SlotPhase::Lost, _) | (_, false) => Some(false),
            _ => None,
        }
    }

    /// Take the published reply out of the slot, if one is there.
    ///
    /// The phase is tested before it is moved, because the operation waiting for
    /// a reply polls this in a loop and every unsuccessful poll would otherwise
    /// move the whole phase out and write it back.
    fn take_published(&mut self) -> Option<SessionReply> {
        if !matches!(self.phase, SlotPhase::Published(_)) {
            return None;
        }
        match std::mem::replace(&mut self.phase, SlotPhase::Idle) {
            SlotPhase::Published(reply) => Some(reply),
            _ => None,
        }
    }

    /// Acknowledge the published reply, or report that none can arrive.
    ///
    /// A session that no longer admits ingress answers closed even with a reply
    /// sitting in the slot, and the reply is dropped rather than handed over: an
    /// access handle that escaped into a task is inert from the moment the route
    /// coordinator revokes it, and taking a real payload after that would be the
    /// escape the revocation exists to prevent.
    fn acknowledge(&mut self) -> Result<Option<SessionReply>, RuntimeError> {
        let published = self.take_published();
        match (published, self.open(), &self.phase, self.terminated) {
            (Some(reply), true, _, _) => Ok(Some(reply)),
            (_, false, _, _) | (None, true, SlotPhase::Lost, _) | (None, true, _, true) => {
                Err(closed())
            }
            (None, true, _, false) => Ok(None),
        }
    }
}

/// The capacity-one command slot connecting the access handle to the driver.
///
/// A hand-built rendezvous rather than a buffered channel because acceptance
/// must be observable: an operation dropped before the driver took its command
/// has to leave no trace, and a buffered send cannot be retracted.
struct CommandSlot {
    inner: Mutex<SlotInner>,
    /// Woken when a command is issued, acknowledged, or admission changes.
    issued: Notify,
    /// Woken when a reply is published or the session closes.
    settled: Notify,
    observer: SessionObserver,
}

impl CommandSlot {
    fn new(observer: SessionObserver) -> Self {
        Self {
            inner: Mutex::new(SlotInner {
                admitting: true,
                attached: true,
                terminated: false,
                abandoned: false,
                phase: SlotPhase::Idle,
                active_metadata: 0,
            }),
            issued: Notify::new(),
            settled: Notify::new(),
            observer,
        }
    }

    fn lock(&self) -> MutexGuard<'_, SlotInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Wake both sides, because every state change is one side's turn to look.
    fn wake(&self) {
        self.issued.notify_one();
        self.settled.notify_one();
    }

    /// Issue one command, or refuse because the session admits none.
    fn issue(&self, kind: CommandKind) -> Result<(), RuntimeError> {
        let mut inner = self.lock();
        let idle = matches!(inner.phase, SlotPhase::Idle);
        match inner.open() && !inner.terminated && idle {
            true => inner.phase = SlotPhase::Issued(kind),
            false => return Err(closed()),
        }
        drop(inner);
        self.issued.notify_one();
        Ok(())
    }

    /// Take one published reply, acknowledging it under the same lock.
    fn take_reply(&self) -> Result<Option<SessionReply>, RuntimeError> {
        let outcome = self.lock().acknowledge();
        if matches!(outcome, Ok(Some(_))) {
            self.issued.notify_one();
        }
        outcome
    }

    /// Wait for the next command the driver may service.
    async fn accept(&self) -> Option<CommandKind> {
        loop {
            let waiting = self.issued.notified();
            let acceptance = self.lock().try_accept();
            match acceptance {
                Acceptance::Accepted(kind) => return Some(self.counted(kind)),
                Acceptance::Closed => return None,
                Acceptance::Pending => waiting.await,
            }
        }
    }

    /// Wait for the route coordinator to close command admission.
    ///
    /// Only the coordinator's own revocation, because that is the one state
    /// change nothing else can answer: the handler has returned, its response is
    /// waiting on this driver, and no command accepted before it can ever be
    /// acknowledged. A lost operation is not this — its session keeps whatever
    /// command it was serving until that command's own boundary.
    async fn revoked(&self) {
        loop {
            let waiting = self.issued.notified();
            if !self.lock().admitting {
                return;
            }
            waiting.await;
        }
    }

    /// Count one accepted command and hand it back.
    fn counted(&self, kind: CommandKind) -> CommandKind {
        observe(&self.observer, SessionMetrics::count_command);
        kind
    }

    /// Publish one reply against the accepted command, if one is still there.
    ///
    /// Synchronous, and held apart from the acknowledgment below, so the phase
    /// between publication and acknowledgment is a state a case can stand in.
    fn publish(&self, reply: SessionReply) -> bool {
        if !self.lock().publish(reply) {
            return false;
        }
        observe(&self.observer, SessionMetrics::count_reply);
        self.settled.notify_one();
        true
    }

    /// Wait for the published reply's acknowledgment.
    ///
    /// `false` is a lost operation: the reply never reached application code, so
    /// the session is abandoned rather than advanced.
    async fn await_acknowledgment(&self) -> bool {
        loop {
            let waiting = self.issued.notified();
            let acknowledged = self.lock().acknowledged();
            match acknowledged {
                Some(settled) => return settled,
                None => waiting.await,
            }
        }
    }

    /// Poison the session: no later operation may succeed.
    fn abandon(&self) {
        self.lock().abandoned = true;
        self.wake();
    }

    /// Close command admission from the route coordinator's side.
    fn revoke(&self) {
        self.lock().admitting = false;
        observe(&self.observer, SessionMetrics::count_revocation);
        self.wake();
    }

    /// Record that the access handle is gone.
    fn detach(&self) {
        self.lock().attached = false;
        self.wake();
    }

    /// Record that the driver has returned.
    fn terminate(&self) {
        self.lock().terminated = true;
        observe(&self.observer, SessionMetrics::count_termination);
        self.wake();
    }

    /// Charge one active field's metadata to the session.
    fn retain_metadata(&self, bytes: usize) {
        let mut inner = self.lock();
        inner.active_metadata += bytes;
        let total = inner.active_metadata;
        drop(inner);
        observe(&self.observer, |metrics| {
            metrics.record_active_metadata(total);
        });
    }

    /// Release one active field's metadata.
    fn release_metadata(&self, bytes: usize) {
        let mut inner = self.lock();
        inner.active_metadata = inner.active_metadata.saturating_sub(bytes);
    }
}

/// The refusal every closed-session operation returns.
fn closed() -> RuntimeError {
    malformed(SESSION_CLOSED)
}

/// The armed cancellation record one in-flight operation carries.
///
/// Cancellation is defined at command acceptance, and this is where that rule
/// lives: a future dropped before the driver took its command retracts it and
/// leaves no trace, while one dropped after abandons the session.
struct OperationGuard<'a> {
    slot: &'a CommandSlot,
    /// Whether losing this operation before acceptance is already terminal.
    /// True for `discard`, which owns an incomplete field from the moment it is
    /// called.
    terminal_before_acceptance: bool,
    armed: bool,
}

impl<'a> OperationGuard<'a> {
    fn new(slot: &'a CommandSlot, terminal_before_acceptance: bool) -> Self {
        Self {
            slot,
            terminal_before_acceptance,
            armed: true,
        }
    }

    /// The operation completed; there is nothing left to cancel.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut inner = self.slot.lock();
        let unaccepted = matches!(inner.phase, SlotPhase::Issued(_) | SlotPhase::Idle);
        match unaccepted && !self.terminal_before_acceptance {
            true => inner.phase = SlotPhase::Idle,
            false => {
                inner.phase = SlotPhase::Lost;
                inner.abandoned = true;
            }
        }
        drop(inner);
        self.slot.wake();
    }
}

/// Run one command from issue through acknowledgment.
async fn run_command(
    slot: &CommandSlot,
    kind: CommandKind,
    terminal_before_acceptance: bool,
) -> Result<SessionReply, RuntimeError> {
    let mut guard = OperationGuard::new(slot, terminal_before_acceptance);
    slot.issue(kind)?;
    loop {
        let waiting = slot.settled.notified();
        match slot.take_reply()? {
            Some(reply) => {
                guard.disarm();
                return Ok(reply);
            }
            None => waiting.await,
        }
    }
}

/// Run one command and take the one answer its operation can act on.
///
/// Every operation fails the same two ways — the session reported a failure, or
/// it published a reply that does not answer this command — so both live here
/// and each operation names only the replies it acts on. The reply is bound
/// before it is mapped, so the borrow the command ran under ends with the call
/// rather than being held across the whole match.
async fn answered<T>(
    slot: &CommandSlot,
    kind: CommandKind,
    terminal_before_acceptance: bool,
    accepted: impl FnOnce(SessionReply) -> Option<T>,
) -> Result<T, RuntimeError> {
    let reply = run_command(slot, kind, terminal_before_acceptance).await?;
    match reply {
        SessionReply::Failed(error) => Err(error),
        answer => accepted(answer).ok_or_else(|| malformed(UNEXPECTED_REPLY)),
    }
}

/// The one body-access capability a streaming multipart handler receives.
///
/// Not cloneable and not constructible by applications: it is the right to ask
/// for the next field, nothing more. Moving it into a task, a channel, or a
/// longer-lived value moves no body, no buffer, and no permit, and the route
/// finalizer can close it from the outside.
pub struct MultipartStream {
    slot: Arc<CommandSlot>,
}

impl MultipartStream {
    /// Advance to the next field in wire order.
    ///
    /// `None` follows a valid closing delimiter and end of body. While the
    /// returned field is alive it borrows this stream, so at most one field
    /// exists at a time and no ingress runs behind it.
    pub async fn next_field(&mut self) -> Result<Option<MultipartField<'_>>, RuntimeError> {
        let metadata = answered(
            &self.slot,
            CommandKind::NextField,
            false,
            |reply| match reply {
                SessionReply::Field(metadata) => Some(Some(metadata)),
                SessionReply::End => Some(None),
                SessionReply::Chunk(_) | SessionReply::FieldEnd | SessionReply::Failed(_) => None,
            },
        )
        .await?;
        Ok(metadata.map(|metadata| MultipartField::new(self, metadata)))
    }
}

impl Drop for MultipartStream {
    fn drop(&mut self) {
        self.slot.detach();
    }
}

impl std::fmt::Debug for MultipartStream {
    /// Show the capability, never the session behind it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MultipartStream")
            .finish_non_exhaustive()
    }
}

/// One field of a streaming multipart body.
///
/// Metadata is owned once, bounded by the header block it was read from, and
/// returned by borrow. Dropping this value before its data ends poisons the
/// session: skipping bytes is `discard`, and it is the only successful skip.
pub struct MultipartField<'a> {
    stream: &'a mut MultipartStream,
    name: Box<str>,
    filename: Option<Box<str>>,
    content_type: Option<Box<str>>,
    metadata_bytes: usize,
    complete: bool,
}

impl<'a> MultipartField<'a> {
    fn new(stream: &'a mut MultipartStream, metadata: FieldMetadata) -> Self {
        stream.slot.retain_metadata(metadata.bytes);
        Self {
            stream,
            name: metadata.name,
            filename: metadata.filename,
            content_type: metadata.content_type,
            metadata_bytes: metadata.bytes,
            complete: false,
        }
    }

    /// The field name this part's content disposition declared.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The uploaded filename, if the part declared one.
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// The part content type, if the part declared one.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Read the next chunk of this field's data.
    ///
    /// Every chunk is non-empty, is at most the configured maximum, and owns its
    /// own allocation: retaining one keeps nothing of the transport frame it
    /// arrived in. `None` ends the field, and every later call answers `None`
    /// again: a completed field issues no further command, so it cannot be
    /// answered with the next field's data.
    pub async fn next_chunk(&mut self) -> Result<Option<Bytes>, RuntimeError> {
        if self.complete {
            return Ok(None);
        }
        let chunk = answered(
            &self.stream.slot,
            CommandKind::NextChunk,
            false,
            |reply| match reply {
                SessionReply::Chunk(chunk) => Some(Some(chunk)),
                SessionReply::FieldEnd => Some(None),
                SessionReply::Field(_) | SessionReply::End | SessionReply::Failed(_) => None,
            },
        )
        .await?;
        self.complete = chunk.is_none();
        Ok(chunk)
    }

    /// Drain the rest of this field under the same bounds, then allow the next.
    ///
    /// This is the only successful skip. It completes one field, not the
    /// request: the handler must keep calling `next_field` until it answers
    /// `None`. A field whose data already ended is already drained, so this
    /// succeeds without issuing a command the next field would answer.
    pub async fn discard(mut self) -> Result<(), RuntimeError> {
        if self.complete {
            return Ok(());
        }
        answered(
            &self.stream.slot,
            CommandKind::Discard,
            true,
            |reply| match reply {
                SessionReply::FieldEnd => Some(()),
                SessionReply::Field(_)
                | SessionReply::Chunk(_)
                | SessionReply::End
                | SessionReply::Failed(_) => None,
            },
        )
        .await?;
        self.complete = true;
        Ok(())
    }
}

impl std::fmt::Debug for MultipartField<'_> {
    /// Show the metadata this field declared, never the session behind it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MultipartField")
            .field("name", &self.name)
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .finish_non_exhaustive()
    }
}

impl Drop for MultipartField<'_> {
    fn drop(&mut self) {
        self.stream.slot.release_metadata(self.metadata_bytes);
        if !self.complete {
            self.stream.slot.abandon();
        }
    }
}

/// The sole owner of one streaming multipart request's framework resources.
///
/// A future the route coordinator owns directly, never a spawned task: when the
/// request is dropped by disconnect, reset, shutdown, or cancellation, the body,
/// the source frame, the parser buffers, and the permit leave through the same
/// stack-owned destruction.
pub(in crate::http) struct MultipartSessionDriver<B> {
    /// The incoming payload, under the one upload owner that bounds its time.
    ///
    /// The owner carries this direction's quiet interval and lifetime, the
    /// request deadlines the admitted head minted, and the cancellation,
    /// shutdown, and peer-lifetime authority. It deliberately carries no payload
    /// maximum: the budget below is this request's single byte accountant.
    upload: Transfer<IncomingSource<B>>,
    budget: BodyBudget,
    /// The admitted permit, held only so that dropping this driver releases it.
    permit: Option<BodyPermit>,
    parser: IncrementalParser,
    /// The one transport frame being consumed, if one is in hand.
    source: Option<Bytes>,
    slot: Arc<CommandSlot>,
    /// What this session has ended as, while it has ended as anything.
    terminal: Option<MultipartTerminal>,
    observer: SessionObserver,
}

impl<B> Drop for MultipartSessionDriver<B> {
    /// Count the admission this session released.
    ///
    /// The permit is held for this drop alone, so the count is written where the
    /// release happens rather than inferred later from a terminal summary.
    fn drop(&mut self) {
        match self.permit {
            Some(_) => observe(&self.observer, SessionMetrics::count_permit_owner_dropped),
            None => {}
        }
    }
}

impl<B> MultipartSessionDriver<B>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Display,
{
    /// Service commands until admission closes, then report the terminal state.
    pub(in crate::http) async fn run(mut self) -> MultipartTerminal {
        while let Some(kind) = self.slot.accept().await {
            self.pause_at(LifecycleCheckpoint::MultipartCommandAccepted)
                .await;
            let Some(reply) = self.serviced(kind).await else {
                break;
            };
            if !self.published(reply).await {
                break;
            }
        }
        self.settle()
    }

    /// Service one command, unless the coordinator revokes admission first.
    ///
    /// `None` is that revocation, and it is selected ahead of the frame the
    /// command is waiting for: the handler has already returned, so this reply
    /// can never be acknowledged, and a peer that stops sending would otherwise
    /// hold the driver — and the response waiting on its terminal summary — open
    /// for as long as it cared to keep the connection.
    async fn serviced(&mut self, kind: CommandKind) -> Option<SessionReply> {
        let slot = Arc::clone(&self.slot);
        tokio::select! {
            biased;
            () = slot.revoked() => None,
            reply = self.service(kind) => Some(reply),
        }
    }

    /// Publish one reply and report whether its operation acknowledged it.
    ///
    /// The payload belongs to application code once the acknowledgment lands, so
    /// the reply term leaves this session's accounting here and nowhere else —
    /// on every exit, including the one where the slot took no reply at all.
    ///
    /// A published failure also ends command admission. The body has already
    /// given its last answer, and a handler that catches the error and asks
    /// again would otherwise have that request polled straight back into a
    /// transport that already failed.
    async fn published(&self, reply: SessionReply) -> bool {
        let failed = matches!(reply, SessionReply::Failed(_));
        let acknowledged = self.delivered(reply).await;
        observe(&self.observer, |metrics| metrics.record_reply(0));
        match failed {
            true => self.slot.abandon(),
            false => {}
        }
        acknowledged
    }

    /// Hand one reply to the operation waiting for it, and wait for its
    /// acknowledgment.
    async fn delivered(&self, reply: SessionReply) -> bool {
        if !self.slot.publish(reply) {
            return false;
        }
        self.pause_at(LifecycleCheckpoint::MultipartReplyPublished)
            .await;
        self.slot.await_acknowledgment().await
    }

    /// Hold at one checkpoint when a controller armed it, and run straight
    /// through when none did.
    async fn pause_at(&self, checkpoint: LifecycleCheckpoint) {
        LifecycleScript::pause_at(self.observer.as_deref(), checkpoint).await;
    }

    /// Run ingress until this command has its one bounded reply.
    async fn service(&mut self, kind: CommandKind) -> SessionReply {
        match self.pump(kind).await {
            Ok(reply) => reply,
            Err(error) => self.record_failure(error),
        }
    }

    /// Advance until an event answers the command in flight.
    async fn pump(&mut self, kind: CommandKind) -> Result<SessionReply, RuntimeError> {
        loop {
            let event = self.next_event().await?;
            let reserved = event.reservation();
            self.parser.release_delivered(reserved);
            match Self::reply_for(kind, event) {
                Some(reply) => return Ok(self.taken(reply, reserved)),
                None => self.record_parser(),
            }
        }
    }

    /// Take one reply's payload over from the parser budget.
    fn taken(&mut self, reply: SessionReply, reserved: usize) -> SessionReply {
        observe(&self.observer, |metrics| metrics.record_reply(reserved));
        if matches!(reply, SessionReply::End) && self.terminal.is_none() {
            self.terminal = Some(MultipartTerminal::Clean);
        }
        self.record_parser();
        reply
    }

    /// Which reply, if any, one event answers a command with.
    ///
    /// Every pairing is named, so an event this command does not answer is a
    /// decision rather than a default: a new event variant has to be placed
    /// here before the crate compiles again.
    fn reply_for(kind: CommandKind, event: ParserEvent) -> Option<SessionReply> {
        match (kind, event) {
            (_, ParserEvent::End) => Some(SessionReply::End),
            (CommandKind::NextField, ParserEvent::Field(metadata)) => {
                Some(SessionReply::Field(metadata))
            }
            (CommandKind::NextChunk, ParserEvent::Chunk(chunk)) => Some(SessionReply::Chunk(chunk)),
            (CommandKind::NextChunk | CommandKind::Discard, ParserEvent::FieldEnd) => {
                Some(SessionReply::FieldEnd)
            }
            (CommandKind::NextField, ParserEvent::Chunk(_) | ParserEvent::FieldEnd)
            | (CommandKind::NextChunk | CommandKind::Discard, ParserEvent::Field(_))
            | (CommandKind::Discard, ParserEvent::Chunk(_)) => None,
        }
    }

    /// Produce the next parser event, pulling frames only when it asks.
    async fn next_event(&mut self) -> Result<ParserEvent, RuntimeError> {
        loop {
            if let Some(event) = self.advance_source()? {
                return Ok(event);
            }
            if !self.pull_frame().await? {
                return self.parser.finish();
            }
        }
    }

    /// Advance the parser over the frame it currently holds.
    ///
    /// A frame that is spent is dropped here rather than kept, so its backing
    /// leaves even while the application still holds chunks copied out of it.
    /// Holding no frame is not the same as needing one: the parser still carries
    /// buffered bytes, and asking for a frame before it has finished with them
    /// would read the end of the body as a truncated one.
    ///
    /// Whether the frame is kept turns on whether it is spent and on nothing
    /// else. What the advance produced is a separate question, and deciding
    /// retention by it made an advance that produced no event responsible for
    /// draining the frame first — an invariant held one module away, where
    /// nothing enforced it and breaking it would have dropped unread payload.
    fn advance_source(&mut self) -> Result<Option<ParserEvent>, RuntimeError> {
        let held = self.source.take();
        let carried = held.is_some();
        let mut source = held.unwrap_or_default();
        let outcome = self.parser.advance(&mut source);
        match source.is_empty() {
            true => self.release_source(source, carried),
            false => self.source = Some(source),
        }
        outcome
    }

    /// Drop the frame this advance was reading, and record its release.
    ///
    /// Only a frame the driver was actually holding counts: an advance that runs
    /// with none in hand builds an empty stand-in, and dropping that releases no
    /// transport allocation.
    fn release_source(&self, source: Bytes, carried: bool) {
        drop(source);
        match carried {
            true => observe(&self.observer, SessionMetrics::count_source_released),
            false => {}
        }
    }

    /// Poll one decoded data frame, or report that the body ended.
    ///
    /// The frame arrives through the upload owner, so every deadline and
    /// cancellation this request runs under is weighed in the same turn the
    /// frame is read in — and by the one declared precedence, not by a rule of
    /// this session's own. Payload bytes are still admitted here, by the one
    /// accountant this request has.
    async fn pull_frame(&mut self) -> Result<bool, RuntimeError> {
        let data = match self.upload.frame().await {
            Ok(None) => return Ok(false),
            Ok(Some(data)) => data,
            Err(failure) => return Err(self.ended(failure)),
        };
        self.count_frame();
        self.admit(data.len())?;
        self.source = Some(data);
        self.pause_at(LifecycleCheckpoint::MultipartIngressAdvanced)
            .await;
        Ok(true)
    }

    /// Record one upload terminal as this session's own, and name its error.
    ///
    /// A transport failure keeps the provenance this session has always reported
    /// it under. Every other cause is the operation's, so it is recorded as the
    /// terminal the route answers rather than folded into a parser fault the
    /// grammar never saw.
    fn ended(&mut self, failure: TransferFailure) -> RuntimeError {
        let error = failure.error();
        match failure.terminal() {
            InboundTerminal::SourceFailure => RuntimeError::RequestBodyUnreadable(
                format!("request body read failed: {error}").into(),
            ),
            InboundTerminal::ShutdownDeadline
            | InboundTerminal::ForcedCancellation
            | InboundTerminal::RouteBodyLimit
            | InboundTerminal::TransferBytes
            | InboundTerminal::BodyIdle
            | InboundTerminal::TransferIdle
            | InboundTerminal::TransferTotal
            | InboundTerminal::RequestTotal
            | InboundTerminal::Disconnect
            | InboundTerminal::ResponseHead => {
                self.record_ended(failure);
                error
            }
        }
    }

    /// Keep the first terminal this session reached, and only the first.
    fn record_ended(&mut self, failure: TransferFailure) {
        match self.terminal {
            None => self.terminal = Some(MultipartTerminal::Ended(failure)),
            Some(_) => {}
        }
    }

    /// Measure one decoded data frame against the admitted maximum.
    ///
    /// Nothing retains a byte of it until this returns: the existing body budget
    /// is the one authority over how much of a request Camber reads.
    fn admit(&mut self, frame_len: usize) -> Result<(), RuntimeError> {
        self.budget
            .admit_frame_within(frame_len)
            .map(drop)
            .map_err(|limit| {
                RuntimeError::RequestBodyLimit(
                    format!("request body exceeds the admitted maximum of {limit} bytes").into(),
                )
            })
    }

    /// Record typed terminal provenance before the failure is replied with.
    ///
    /// The diagnostic the terminal keeps is the one this error carries, so the
    /// finalizer's restatement rebuilds this exact error instead of a second
    /// `Display` pass over it.
    fn record_failure(&mut self, error: RuntimeError) -> SessionReply {
        let (failure, diagnostic) = MultipartFailure::split(error);
        match self.terminal {
            None => {
                self.terminal = Some(MultipartTerminal::ParserFailure(
                    failure,
                    diagnostic.clone(),
                ));
            }
            Some(_) => {}
        }
        SessionReply::Failed(failure.restate(diagnostic))
    }

    /// Close the session and report what it ended as.
    ///
    /// A session that reached no terminal state of its own ended with payload
    /// unread, which is abandonment: the coordinator revoked admission, the
    /// handle went away, or an operation was lost.
    fn settle(mut self) -> MultipartTerminal {
        self.slot.terminate();
        self.record_parser();
        self.terminal.take().unwrap_or(MultipartTerminal::Abandoned)
    }

    /// Publish what the parser is holding.
    fn record_parser(&self) {
        observe(&self.observer, |metrics| {
            metrics.record_parser(self.parser.retained_bytes(), self.parser.peak_bytes());
        });
    }

    /// Count one polled payload frame.
    fn count_frame(&self) {
        observe(&self.observer, SessionMetrics::count_frame);
    }
}

/// The revocation authority one route coordinator keeps to itself.
///
/// Held where the access handle cannot reach it, so handler completion closes
/// command admission and drives the session to a terminal state regardless of
/// where the handle ended up.
pub(in crate::http) struct MultipartRevocation {
    slot: Arc<CommandSlot>,
}

impl MultipartRevocation {
    /// Close command admission. Any escaped handle becomes inert.
    pub(in crate::http) fn revoke(&self) {
        self.slot.revoke();
    }
}

/// Build one streaming multipart session over an admitted body.
///
/// The three values it hands back are the whole ownership split: the driver owns
/// the request, the revocation belongs to the coordinator, and the stream is all
/// the application ever sees.
pub(in crate::http) fn open<B>(
    body: B,
    admitted: AdmittedBody,
    boundary: &str,
    limits: MultipartLimits,
    observer: SessionObserver,
    upload: TransferOwner,
) -> (
    MultipartStream,
    MultipartRevocation,
    MultipartSessionDriver<B>,
)
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Display,
{
    let budget = BodyBudget::new(&admitted);
    let slot = Arc::new(CommandSlot::new(observer.clone()));
    let driver = MultipartSessionDriver {
        upload: upload.deadlines_only().over(IncomingSource::new(body)),
        budget,
        permit: admitted.permit,
        parser: IncrementalParser::new(boundary, limits),
        source: None,
        slot: Arc::clone(&slot),
        terminal: None,
        observer,
    };
    let revocation = MultipartRevocation {
        slot: Arc::clone(&slot),
    };
    (MultipartStream { slot }, revocation, driver)
}
