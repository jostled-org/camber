//! One streaming multipart route's request lifecycle.
//!
//! The route class this answers is chosen by the same trie and host resolver
//! every other route is chosen by. What is specialized is the order after that:
//! admission decides how much of the payload may be read, the router's own
//! middleware gates the metadata-only request, the declared boundary is checked
//! before a single frame is polled, and only then does a session exist at all.
//!
//! Everything the framework owns for that session — the incoming body, the
//! admitted budget, the permit, the parser buffers, and the one source frame —
//! lives in a driver future this stack owns beside the handler's. Nothing is
//! spawned, so a request dropped by disconnect, reset, shutdown, or cancellation
//! takes all of it with it. When the handler ends, this file revokes command
//! admission first and waits for the driver's terminal summary second, so the
//! response is selected against a session that has already released everything,
//! wherever the handler left its access handle.

use super::body::HyperResponseBody;
use super::body_admission::{self, AdmittedBody, ResolvedBodyPlan};
use super::dispatch::{FrozenRouter, PreBodyScope, StreamingMultipartTarget};
use super::handle::{
    RequestDispatch, answer, answer_inbound_failure, answer_rejected, gate_outcome,
};
use super::mock::{LifecycleCheckpoint, LifecycleScript};
use super::multipart::{
    MultipartCompletion, MultipartRevocation, MultipartSessionDriver, MultipartTerminal,
    SessionObserver, open, request_boundary,
};
use super::operation::{InboundFailure, OperationStage};
use super::rejection::{
    HANDLER, MULTIPART_SESSION, ProducerKinds, Rejected, RejectionScope, RequestIdentity,
};
use super::request::{RequestHead, RequestOrigin};
use super::response::HandlerOutcome;
use super::{Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// The handler future one registration hands back.
type HandlerFuture = Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>;

/// Answer one streaming multipart request.
///
/// The clock, the connection context, and the peer identity arrive from
/// `handle_request`, so this class is recorded in the same histogram every other
/// one is: a second `Instant::now()` here would leave its buckets incomparable
/// with the rest.
///
/// The child router arrives from classification rather than being resolved
/// again. Only a resolved child can produce this class, and the authority parse
/// and host-table search that selected it are exactly the two the gate below
/// would otherwise repeat.
pub(super) async fn dispatch_streaming_multipart(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    target: StreamingMultipartTarget,
    router: Option<&FrozenRouter>,
    pre_body: &PreBodyScope,
    request_dispatch: &RequestDispatch<'_>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &RequestDispatch {
        ctx,
        origin,
        lifecycle,
        start,
        operation,
        ..
    } = request_dispatch;
    let StreamingMultipartTarget {
        registration,
        params,
        plan,
    } = target;
    let limits = registration.limits();
    // The route and dispatch class classification established, carried into
    // every refusal this path can raise: admission's, the gate's, the
    // boundary's, the session's, and the handler's alike.
    let scope = pre_body.scope(RequestIdentity::from_head(
        &origin,
        hyper_req.method(),
        hyper_req.uri(),
    ));
    let observer = lifecycle.script();
    let admitted = match admit(&hyper_req, &plan, origin, observer.as_ref()).await {
        Ok(admitted) => admitted,
        Err(rejected) => return Ok(answer_rejected(ctx, &scope, rejected, start)),
    };
    let request = RequestHead::from_hyper_request(&hyper_req, origin).to_request(Some(params));
    operation.observe(observer.as_deref(), OperationStage::Middleware);
    // Closing, because the payload this request declared was never read: the
    // next request on an HTTP/1 connection would otherwise be framed out of it.
    let gate = gated(&request, router, &scope);
    let answering = super::handle::gate_within_total(gate, request_dispatch, &scope, |blocked| {
        scope.closing(blocked)
    });
    if let Some(answered) = answering.await {
        return Ok(answered);
    }
    let boundary =
        match request_boundary(request.header("content-type"), limits.max_boundary_bytes()) {
            Ok(boundary) => boundary,
            Err(refused) => {
                let response = Settled::framework(refused).respond(&scope);
                return Ok(answer(ctx, response, start, &scope));
            }
        };

    let session = opened(
        hyper_req.into_body(),
        &request,
        &registration,
        Opening {
            admitted,
            boundary: &boundary,
            limits,
            observer: observer.clone(),
            operation,
        },
    );
    // The session reads this operation's request payload and produces its
    // response head, so the whole of it spends the one request total the
    // admitted head minted.
    let settled = match super::handle::within_total(session, operation).await {
        Ok(settled) => settled,
        Err(rejected) => return Ok(answer_rejected(ctx, &scope, rejected, start)),
    };
    operation.observe(observer.as_deref(), OperationStage::ResponseHead);
    Ok(match settled {
        Answered::Settled(settled) => answer(ctx, settled.respond(&scope), start, &scope),
        // The upload owner's cause carries its own disposition: a mapped one owes
        // the peer this route's mapper once, and a silent one owes it nothing.
        Answered::Ended(failure) => answer_inbound_failure(ctx, &scope, failure, start),
    })
}

/// What one session is opened over, beside the body and the handler.
///
/// Grouped because the open is one decision over all of it: the admitted budget,
/// the declared boundary, the registration's grammar bounds, the listener's
/// observer, and the operation whose transfer policy the upload runs under move
/// together or not at all.
struct Opening<'a> {
    admitted: AdmittedBody,
    boundary: &'a str,
    limits: super::multipart::MultipartLimits,
    observer: SessionObserver,
    operation: &'a super::operation::OperationEnvelope,
}

/// Open one session over an admitted payload and enter its handler.
///
/// The upload owner is resolved here, before a frame is polled: the route's
/// transfer policy narrowed by nothing else, because a multipart registration
/// states its grammar bounds rather than a transfer budget of its own.
///
/// The handler is entered against a borrow that ends here. The future it hands
/// back is `'static`, so nothing the session holds borrows from the request, and
/// the request goes on owning this response's lifetime signal for as long as its
/// caller's frame does.
fn opened(
    body: hyper::body::Incoming,
    request: &Request,
    registration: &super::trie::MultipartRegistration,
    opening: Opening<'_>,
) -> impl Future<Output = Answered> {
    let Opening {
        admitted,
        boundary,
        limits,
        observer,
        operation,
    } = opening;
    let upload = operation.upload(super::TransferBudget::unbounded(), observer.clone());
    let (stream, revocation, driver) =
        open(body, admitted, boundary, limits, observer.clone(), upload);
    operation.observe(observer.as_deref(), OperationStage::Body);
    Session {
        handler: registration.enter(request, stream),
        driver,
        revocation,
        observer,
    }
    .run(operation.budget())
}

/// Admit this request's payload before anything reads a byte of it.
///
/// The head is borrowed here and nowhere else on the refusal path: admission
/// answers from what the head established, and the borrow ends before the
/// incoming body moves into a session.
async fn admit(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    plan: &ResolvedBodyPlan,
    origin: RequestOrigin<'_>,
    observer: Option<&Arc<LifecycleScript>>,
) -> Result<AdmittedBody, Rejected> {
    let head = RequestHead::from_hyper_request(hyper_req, origin);
    body_admission::admit(plan, &head, observer).await
}

/// Run this router's middleware over the metadata-only request.
///
/// The request handed to the chain is the one the handler will be given, so a
/// gate and a handler cannot disagree about what they were asked about. What
/// that chain's answer means is [`gate_outcome`]'s to say, shared with every
/// other class that runs a gate.
async fn gated(
    request: &Request,
    router: Option<&FrozenRouter>,
    scope: &RejectionScope,
) -> Option<Response> {
    gate_outcome(router.and_then(|router| router.middleware_gate(request, scope))).await
}

/// One running multipart request: its handler, its driver, and the revocation
/// the handler cannot reach.
///
/// Held together because they end together. The revocation is the coordinator's
/// alone, which is what makes handler completion release the session even when
/// the access handle was moved into a task, a channel, or a longer-lived value.
struct Session {
    handler: HandlerFuture,
    driver: MultipartSessionDriver<hyper::body::Incoming>,
    revocation: MultipartRevocation,
    observer: SessionObserver,
}

impl Session {
    /// Run this session to its end and report what it settled on.
    ///
    /// The response is not selected here, and nothing about it is decided before
    /// the driver has returned: what this hands back is the outcome precedence
    /// chose over a session that has already released its body, its source
    /// frame, its parser buffers, and its permit.
    async fn run(self, budget: super::RequestBudget) -> Answered {
        let Self {
            handler,
            driver,
            revocation,
            observer,
        } = self;
        let (outcome, terminal) = paired(handler, driver, &revocation, observer.as_deref()).await;
        LifecycleScript::pause_at(
            observer.as_deref(),
            LifecycleCheckpoint::BeforeMultipartResponseSelection,
        )
        .await;
        Answered::of(terminal.completion(), outcome, budget)
    }
}

/// What one finished multipart request answers with.
///
/// The two arms are two different authorities. A settled session is answered by
/// the precedence between its own terminal and its handler's outcome. A session
/// the operation's owner ended is answered by the declared inbound precedence,
/// which already named the cause and its disposition — a handler cannot turn a
/// crossed deadline into a committed response.
enum Answered {
    Settled(Settled),
    Ended(InboundFailure),
}

impl Answered {
    /// Apply the precedence one finished session and its handler settle on.
    fn of(
        completion: MultipartCompletion,
        outcome: HandlerOutcome,
        budget: super::RequestBudget,
    ) -> Self {
        match completion {
            MultipartCompletion::Ended(failure) => Self::Ended(InboundFailure::of(
                failure.terminal(),
                budget,
                failure.refusal(),
            )),
            settled => Self::Settled(Settled::of(settled, outcome)),
        }
    }
}

/// Poll the handler and the driver together, then close the session behind it.
///
/// Revocation happens before the driver is waited on, never after: a handler
/// that kept its access handle alive somewhere else would otherwise leave the
/// driver waiting for a command that will never come, and this request would
/// never answer at all.
async fn paired(
    handler: HandlerFuture,
    driver: MultipartSessionDriver<hyper::body::Incoming>,
    revocation: &MultipartRevocation,
    observer: Option<&LifecycleScript>,
) -> (HandlerOutcome, MultipartTerminal) {
    let mut handler = handler;
    let mut running = std::pin::pin!(driver.run());
    let mut settled = None;
    let outcome = std::future::poll_fn(|cx| {
        advance(running.as_mut(), &mut settled, cx);
        handler.as_mut().poll(cx)
    })
    .await;
    LifecycleScript::pause_at(observer, LifecycleCheckpoint::MultipartHandlerCompleted).await;
    revocation.revoke();
    let terminal = match settled {
        Some(terminal) => terminal,
        None => running.await,
    };
    LifecycleScript::pause_at(observer, LifecycleCheckpoint::MultipartDriverTerminated).await;
    (outcome, terminal)
}

/// Advance the driver while it is still running.
///
/// Its terminal summary is kept the moment it returns one, so a driver that
/// finished before its handler did is never polled again.
fn advance(
    driver: Pin<&mut impl Future<Output = MultipartTerminal>>,
    settled: &mut Option<MultipartTerminal>,
    cx: &mut Context<'_>,
) {
    let polled = match settled.is_some() {
        true => Poll::Pending,
        false => driver.poll(cx),
    };
    match polled {
        Poll::Ready(terminal) => *settled = Some(terminal),
        Poll::Pending => {}
    }
}

/// What one finished multipart request answers with.
struct Settled {
    outcome: HandlerOutcome,
    /// Which producer classifies a failure in that outcome.
    kinds: ProducerKinds,
    /// Whether payload the peer sent was left unread.
    unread: bool,
}

impl Settled {
    /// The framework's own failure answers this request.
    fn framework(failure: crate::RuntimeError) -> Self {
        Self {
            outcome: Err(failure),
            kinds: MULTIPART_SESSION,
            unread: true,
        }
    }

    /// Apply the precedence one finished session and its handler settle on.
    ///
    /// A recorded parser, byte-limit, or transport failure outranks whatever
    /// the handler said: a handler cannot turn a refused payload into a
    /// committed response by catching the error it was handed. Abandonment is
    /// the weaker claim — a handler that reported its own failure keeps it, and
    /// only one that claimed success is answered with the incomplete body.
    fn of(completion: MultipartCompletion, outcome: HandlerOutcome) -> Self {
        match (completion, outcome) {
            (MultipartCompletion::Complete, outcome) => Self {
                outcome,
                kinds: HANDLER,
                unread: false,
            },
            (MultipartCompletion::Failed(failure), _) => Self::framework(failure),
            (MultipartCompletion::Incomplete(_), Err(error)) => Self {
                outcome: Err(error),
                kinds: HANDLER,
                unread: true,
            },
            (MultipartCompletion::Incomplete(incomplete), Ok(_)) => Self::framework(incomplete),
            // Answered by [`Answered::of`], which reaches this precedence only
            // for the completions the session itself owns. Named rather than
            // wildcarded so a later completion cannot fall through into the
            // handler's outcome.
            (MultipartCompletion::Ended(ended), _) => Self::framework(ended.error()),
        }
    }

    /// Turn this settled request into the response the peer is given.
    ///
    /// The mapper runs at most once, on the one outcome precedence selected. A
    /// request whose payload was left unread also states its transport
    /// disposition here: HTTP/1 cannot frame another request behind bytes
    /// nothing read, and the scope owns that version question for every refusal
    /// Camber answers.
    fn respond(self, scope: &RejectionScope) -> Response {
        let response = scope.resolve(self.outcome, self.kinds);
        match self.unread {
            true => scope.closing(response),
            false => response,
        }
    }
}
