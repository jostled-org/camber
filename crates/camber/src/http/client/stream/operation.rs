//! The streaming terminal of one outbound operation, and the one task that
//! owns its response.
//!
//! A buffered call ends when its caller has the whole answer, so its caller's
//! future can own it. A streamed call returns at the head and lives on while
//! the caller holds an unread handle. Its deadlines and its runtime's stop
//! request must still end it then, with no poll from the caller. One task
//! therefore owns the operation from its first poll to the release of its
//! source.
//!
//! The first poll of the send fixes everything the task runs under: the
//! stream's bounds, the operation deadline, the runtime that owns the task or
//! the absence of one, and the caller's trace context. The task is admitted
//! before it is given anything, so a refused admission holds no request.
//!
//! The task runs the shared final-head coordinator and then the body engine,
//! inline. It starts no second task. The lifetime, the attempt timer, the
//! terminal record, and the transport move from the first to the second by
//! value, so the handoff restarts no timer and records no success.
//!
//! Under a Camber runtime the task is a child of that runtime's root scope.
//! The root joins it, and no handle does. On bare Tokio the pending send and
//! then the body handle own the task, and a drop of either aborts it.

use super::super::super::body_admission::declared_length_exceeds_limit;
use super::super::super::mock::{self, LifecycleScript, TransferOwnerEdge};
use super::super::body::{BareOwner, BodyAccess, BodyBudget, BodyOwner, ClientBody};
use super::super::completion::{Completion, Terminal};
use super::super::description::Description;
use super::super::error::{ClientError, ClientInput};
use super::super::handoff::FinalHead;
use super::super::head::ResponseHead;
use super::super::intercept::{admit_recorded, mock_answer};
use super::super::lifetime::{Lifetime, Stop};
use super::super::operation::{final_head, first_poll};
use super::super::policy::OperationPolicy;
use super::super::response::ClientResponse;
use super::super::reusable::Client;
use super::super::transport::{Dialer, OperationTransport};
use super::response::ClientStreamResponse;
use super::source::{OwnerEnded, Source, StreamRecord};
use crate::RuntimeError;
use crate::runtime_state::{self as runtime, LifecycleSignals};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// The failure of a send whose task ended before it answered.
///
/// The task was aborted or it panicked. The caller learns only that the
/// transfer failed.
const OWNER_LOST: RuntimeError = RuntimeError::Client(ClientError::Transport);

/// The refusal of a stream with no finite byte maximum or idle interval.
const UNBOUNDED_STREAM: RuntimeError =
    RuntimeError::Client(ClientError::InvalidRequest(ClientInput::StreamBudget));

/// Run one described call to its response head or its failure.
///
/// This is the operation's first poll, and the terminal record begins here.
/// A call refused before its task starts settles the record here. A started
/// call moves the record into its task, and the task settles it after the
/// source is released.
///
/// The returned future owns the pending send. Dropping it ends the operation:
/// the task reads the lost waiter and releases whatever it holds.
pub(in crate::http::client) async fn operate(
    client: &Client,
    description: Description,
    policy: OperationPolicy,
) -> Result<ClientStreamResponse, RuntimeError> {
    let completion = Completion::begin(description.method(), description.url());
    let launched = match Launch::admitted(client, description, policy) {
        Ok(launch) => launch.start(completion),
        Err(refusal) => Err((refusal, completion)),
    };
    match launched {
        Ok(pending) => pending.head().await,
        Err((refusal, completion)) => {
            let refused = Err(refusal);
            completion.settle(Terminal::of(&refused));
            refused
        }
    }
}

/// The finite bounds one streamed body is read under.
#[derive(Clone, Copy)]
struct StreamBounds {
    /// The most payload bytes the body may deliver.
    limit: usize,
    /// How long one accepted demand may wait for a nonempty chunk.
    idle: Duration,
}

impl StreamBounds {
    /// The bounds of a request, or the refusal of one that removed either.
    ///
    /// A buffered call may run with no maximum or no quiet interval. A stream
    /// may not: its caller can hold it for as long as it likes.
    fn of(description: &Description, policy: &OperationPolicy) -> Result<Self, RuntimeError> {
        match (description.response_limit(), policy.idle) {
            (Some(limit), Some(idle)) => Ok(Self { limit, idle }),
            _ => Err(UNBOUNDED_STREAM),
        }
    }
}

/// Where the task of one streamed call gets its answer.
enum Answerer {
    /// The network, through this dialer.
    Network(Dialer<'static>),
    /// A registered mock. Its answer is already whole.
    Mock(ClientResponse),
}

/// Everything one admitted call runs under, fixed at its first poll.
struct Work {
    description: Description,
    policy: OperationPolicy,
    lifetime: Lifetime,
    bounds: StreamBounds,
    answerer: Answerer,
}

/// What the task hands the waiting send: the head, and the sole access to the
/// body.
///
/// A value that is never received is dropped, and the drop of its access
/// abandons the body. A head delivery that fails therefore leaves no orphan.
struct Handed {
    head: ResponseHead,
    access: BodyAccess,
}

/// What a waiting send receives: the handoff, or the failure of the operation.
type Answer = Result<Handed, RuntimeError>;

/// What an admitted task is started with.
struct Started {
    work: Work,
    completion: Completion,
    /// The way back to the waiting send. Its close is the send's
    /// cancellation.
    answer: oneshot::Sender<Answer>,
}

/// The admitted task of one streamed call, before it holds anything.
///
/// The task waits for its start. Dropping the seat drops the only sender, so
/// a task that is never started ends at once.
struct Seat {
    start: oneshot::Sender<Started>,
    /// The task, when no Camber runtime owns it.
    owner: Option<BareOwner>,
}

/// Wait for the start, then run the operation as the whole of this task.
async fn seated(start: oneshot::Receiver<Started>) {
    if let Ok(started) = start.await {
        started.run().await;
    }
}

impl Seat {
    /// Admit the task to the Camber runtime that polls this send, or spawn it
    /// on `executor` when there is none.
    ///
    /// The runtime is read once, here. A task it admits is a child of its
    /// root scope and stops on its signals, wherever the body handle moves
    /// afterwards. A closed scope refuses the task, and nothing was given to
    /// it. With no Camber runtime, none is made.
    ///
    /// The second value is where the operation reads its stop request.
    fn admit(executor: &tokio::runtime::Handle) -> Result<(Self, Stop), RuntimeError> {
        let (start, started) = oneshot::channel();
        match runtime::try_current_runtime() {
            Some(root) => {
                root.admit_internal_async(None, seated(started))?;
                let stop = Stop::Captured(LifecycleSignals::from_runtime(&root));
                Ok((Self { start, owner: None }, stop))
            }
            None => {
                let owner = BareOwner::of(executor.spawn(seated(started)));
                let seat = Self {
                    start,
                    owner: Some(owner),
                };
                Ok((seat, Stop::Never))
            }
        }
    }
}

/// One streamed call that passed every check of its first poll.
struct Launch {
    seat: Seat,
    work: Work,
}

impl Launch {
    /// Check one described call, and admit its task.
    ///
    /// The checks that need more than one field run first, so a refused
    /// description sends nothing. The stream's bounds are read next, before
    /// any effect. The executor is checked after that, so a call polled
    /// outside Tokio returns `NoRuntime` and spawns nothing. The deadline is
    /// fixed then, and the client's egress policy reads the scheme, under a
    /// mock too. Only then is the task admitted.
    ///
    /// A mock answers without a lookup, and it is no evidence of where a
    /// connection would go. A call the network answers reads the caller's
    /// trace context here, in the task that polls the send.
    fn admitted(
        client: &Client,
        mut description: Description,
        policy: OperationPolicy,
    ) -> Result<Self, RuntimeError> {
        description.admit()?;
        let bounds = StreamBounds::of(&description, &policy)?;
        let (executor, lifetime) = first_poll(client, &description, &policy)?;
        let (seat, stop) = Seat::admit(&executor)?;
        let answerer = match mock_answer(&description) {
            Some(canned) => Answerer::Mock(canned),
            None => {
                description.carry_trace_context(policy.trace_context);
                Answerer::Network(client.stream_dialer()?)
            }
        };
        Ok(Self {
            seat,
            work: Work {
                description,
                policy,
                lifetime: lifetime.stopped_by(stop),
                bounds,
                answerer,
            },
        })
    }

    /// Give the admitted task its work and the terminal record.
    ///
    /// A task that is already gone gives both back. The record is returned
    /// with the failure, so the caller settles it.
    fn start(self, completion: Completion) -> Result<PendingHead, (RuntimeError, Completion)> {
        let (answer, answered) = oneshot::channel();
        let started = Started {
            work: self.work,
            completion,
            answer,
        };
        match self.seat.start.send(started) {
            Ok(()) => Ok(PendingHead {
                answered,
                owner: self.seat.owner,
            }),
            Err(unseated) => Err((OWNER_LOST, unseated.completion)),
        }
    }
}

/// The waiting side of one started send, and its cancellation guard.
///
/// Dropping it closes the way back, which the task reads as the send's
/// cancellation. On bare Tokio it also aborts the task. A head that arrives
/// beside the drop is dropped with it, and that abandons the body.
struct PendingHead {
    answered: oneshot::Receiver<Answer>,
    /// The task, when no Camber runtime owns it. It moves into the body
    /// handle with the head.
    owner: Option<BareOwner>,
}

impl PendingHead {
    /// Wait for the head or the failure of the operation.
    ///
    /// A failure arrives after the task released what it held.
    async fn head(self) -> Result<ClientStreamResponse, RuntimeError> {
        let Self { answered, owner } = self;
        match answered.await {
            Ok(Ok(Handed { head, access })) => Ok(ClientStreamResponse::new(
                head,
                ClientBody::owned(access, owner),
            )),
            Ok(Err(failure)) => Err(failure),
            Err(_) => Err(OWNER_LOST),
        }
    }
}

/// A final answer whose body is unread, with what that body is read under.
struct Opened {
    head: ResponseHead,
    source: Source,
    /// The operation's own lifetime. The handoff restarts no deadline.
    lifetime: Lifetime,
    /// When the final attempt expires, as its send armed it.
    attempt: Option<Instant>,
    /// The transport a network answer arrived on. It is released after the
    /// source it served.
    transport: Option<OperationTransport<'static>>,
    /// The counts of this stream, when a test watches its peer.
    record: Option<Arc<StreamRecord>>,
    /// The watcher of this stream's peer, when a test holds its owner.
    watcher: Option<Arc<LifecycleScript>>,
}

impl Opened {
    /// Open a mock's answer that was admitted under the request's maximum.
    ///
    /// A mock body is already whole, so its length is its declared length.
    /// The attempt timer starts here: a mock has no send to arm it.
    fn canned(admitted: ClientResponse, lifetime: Lifetime, policy: &OperationPolicy) -> Self {
        let (head, body) = admitted.into_parts();
        Self {
            head,
            source: Source::canned(body),
            attempt: lifetime.attempt_bound(policy.attempt),
            lifetime,
            transport: None,
            record: None,
            watcher: None,
        }
    }

    /// Open the final head of a network answer under the request's maximum.
    ///
    /// The transport's effective body length is read before any body byte. A
    /// length over `limit` is refused here: the response is released, and
    /// then the transport it arrived on. A HEAD answer and a bodyless status
    /// have an effective length of zero, whatever their head declares.
    fn wire(head: FinalHead<'static>, limit: usize) -> Result<Self, RuntimeError> {
        let FinalHead {
            response,
            lifetime,
            attempt,
            transport,
            // The stream reads its finite maximum from its own bounds.
            limit: _,
        } = head;
        let crossed = response
            .content_length()
            .is_some_and(|declared| declared_length_exceeds_limit(declared, limit));
        match crossed {
            true => {
                drop(response);
                drop(transport);
                Err(RuntimeError::Client(ClientError::ResponseTooLarge {
                    limit,
                }))
            }
            false => {
                let watcher = response.remote_addr().and_then(mock::lifecycle_script);
                let record = watcher.as_ref().map(|script| script.streams().open());
                Ok(Self {
                    head: ResponseHead::of(&response),
                    source: Source::wire(response, record.clone()),
                    lifetime,
                    attempt,
                    transport: Some(transport),
                    record,
                    watcher,
                })
            }
        }
    }

    /// Hand the head to the waiting send, then serve the body to its end.
    ///
    /// The body owner takes the terminal record. It settles the record on the
    /// body's own fact, after it released the source and before it
    /// acknowledges. A send that no longer waits drops the handoff, which
    /// abandons the body, and the owner then ends on that fact.
    ///
    /// A test that watches the peer can hold the owner before the delivery.
    /// With no watcher the hold is nothing.
    async fn serve(
        self,
        bounds: StreamBounds,
        completion: Completion,
        answer: oneshot::Sender<Answer>,
    ) {
        let ended = OwnerEnded::of(self.record.clone());
        let transport = self.transport;
        let budget = BodyBudget {
            limit: bounds.limit,
            counted: 0,
            idle: bounds.idle,
            lifetime: self.lifetime,
            attempt: self.attempt,
        };
        let (owner, access) = BodyOwner::new(self.source, budget, Some(completion));
        if let Some(record) = &self.record {
            record.watch_body(access.watch());
        }
        let handed = Handed {
            head: self.head,
            access,
        };
        let undelivered = TransferOwnerEdge::BeforeHeadDelivery;
        LifecycleScript::pause_at_transfer(self.watcher.as_deref(), undelivered).await;
        // An undelivered handoff comes back here and is dropped.
        drop(answer.send(Ok(handed)));
        owner.run().await;
        drop(transport);
        drop(ended);
    }
}

impl Work {
    /// Run the call to its opened final answer.
    ///
    /// A mock answers at once. The network answers through the shared
    /// final-head coordinator, which follows every retry and every redirect
    /// and returns at the final head. Nothing sends after it.
    async fn opened(self, completion: &mut Completion) -> Result<Opened, RuntimeError> {
        let Self {
            description,
            policy,
            lifetime,
            bounds,
            answerer,
        } = self;
        match answerer {
            Answerer::Mock(canned) => {
                let admitted = admit_recorded(canned, Some(bounds.limit), completion)?;
                Ok(Opened::canned(admitted, lifetime, &policy))
            }
            Answerer::Network(dialer) => {
                let transport = dialer.open(description.url(), &lifetime).await?;
                let head = final_head(transport, dialer, description, policy, lifetime, completion)
                    .await?;
                Opened::wire(head, bounds.limit)
            }
        }
    }
}

impl Started {
    /// Run the operation as the whole of its task.
    ///
    /// A send that stops waiting before the head ends the operation: the
    /// attempt in flight is dropped, and the record's own drop reports the
    /// abandonment. The select names no winner. A head that arrives beside
    /// the cancellation is released with it.
    ///
    /// A failure before the head settles the record first and answers the
    /// send second, so the send returns after everything was released.
    async fn run(self) {
        let Self {
            work,
            mut completion,
            mut answer,
        } = self;
        let bounds = work.bounds;
        let opened = tokio::select! {
            biased;
            () = answer.closed() => return,
            opened = work.opened(&mut completion) => opened,
        };
        match opened {
            Ok(opened) => opened.serve(bounds, completion, answer).await,
            Err(failure) => {
                let refused: Answer = Err(failure);
                completion.settle(Terminal::of(&refused));
                // A send that no longer waits needs no answer.
                drop(answer.send(refused));
            }
        }
    }
}
