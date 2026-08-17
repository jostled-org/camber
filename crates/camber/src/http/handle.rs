use super::body::HyperResponseBody;
use super::body_admission::{BodyBudget, BodyPermit, ResolvedBodyPlan};
use super::disconnect::{ConnectionLiveness, ResponseLifetime};
use super::dispatch::{
    AsyncDispatch, Classified, FrozenRouter, HeadUpgrade, PreBodyScope, RouteClass, Routed,
};
#[cfg(feature = "profiling")]
use super::internal_routes::match_profiling_route;
use super::internal_routes::{
    build_internal_handler, invoke_internal_route, match_internal_route_from_path,
};
use super::operation::{
    InboundFailure, InboundGuard, InboundReady, InboundTerminal, OperationEnvelope, OperationStage,
};
use super::record::{count_rejection, record_scoped};
use super::rejection::{
    HANDLER, Rejected, RejectionProtocol, RejectionScope, RequestId, RequestIdentity,
};
use super::request::{RequestHead, RequestOrigin};
use super::router::{DispatchResult, GateCheck, ServerDispatch, gate_result};
use super::server_lifecycle::ConnectionLifecycle;
use super::streaming::{
    dispatch_streaming_proxy, handle_proxy_stream_response, handle_sse, handle_stream_response,
};
#[cfg(feature = "ws")]
use super::ws_proxy::{self, WsUpgrade};
use super::{BufferConfig, Request, Response};
use crate::resource::HealthState;
use crate::runtime_state::RuntimeInner;
use std::convert::identity;
use std::sync::Arc;

#[cfg(feature = "grpc")]
use super::grpc_support::is_grpc_request;

pub(super) struct ConnCtx {
    pub(super) tracing_enabled: bool,
    pub(super) metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    #[cfg(feature = "profiling")]
    pub(super) profiling_enabled: bool,
    pub(super) sse_buffer_size: usize,
    #[cfg(feature = "ws")]
    pub(super) ws_buffer_size: usize,
    pub(super) health_state: Option<HealthState>,
    pub(super) is_tls: bool,
    /// The bounds this server serves under, already narrowed against the
    /// runtime that contains it. Every request this connection carries resolves
    /// its own budgets under this one value.
    pub(super) policy: super::ServerPolicy,
}

impl ConnCtx {
    /// Build from a running Camber runtime and buffer configuration.
    pub(super) fn from_runtime(
        rt: &Arc<RuntimeInner>,
        buffers: BufferConfig,
        is_tls: bool,
        policy: super::ServerPolicy,
    ) -> Self {
        Self {
            tracing_enabled: rt.config.tracing_enabled,
            metrics_handle: rt.metrics_handle.clone(),
            #[cfg(feature = "profiling")]
            profiling_enabled: rt.config.profiling_enabled,
            sse_buffer_size: buffers.sse_buffer_size,
            #[cfg(feature = "ws")]
            ws_buffer_size: buffers.ws_buffer_size,
            health_state: rt.health_state.clone(),
            is_tls,
            policy,
        }
    }
}

/// A request refused before it ever reached dispatch, and the identity to
/// record it under.
///
/// The refusal alone was not enough. Every pre-dispatch refusal left with no
/// method and no path to name, so nothing recorded it: a peer oversizing every
/// body could drive a server to shed all of its traffic while
/// `http_requests_total` and `http_request_duration_seconds` stayed flat.
struct Refused {
    failure: InboundFailure,
    method: hyper::Method,
    uri: hyper::Uri,
}

/// What a request-building step answers with.
///
/// The refusal is boxed: a rejection beside a `Uri` and a `Method` is well over
/// `clippy::result_large_err`'s threshold, and that weight would otherwise ride
/// the success path's stack on every request to describe a refusal that usually
/// never fires.
type Building = Result<DispatchInput, Box<Refused>>;

/// Collect the hyper body with a size limit and build a Camber Request.
///
/// `method` arrives already parsed, from the classification that read this
/// request's head. Only a matched route reaches here, so the method is one
/// Camber routes on, and the URI moves into whichever of the two outcomes needs
/// it instead of being copied for a refusal that usually never fires.
async fn collect_body_limited(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    buffered: BufferedRead,
    operation: &OperationEnvelope,
    origin: RequestOrigin<'_>,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
) -> Result<Request, Box<Refused>> {
    let (parts, body) = hyper_req.into_parts();
    let BufferedRead {
        method,
        budget,
        permit,
    } = buffered;
    let body_bytes = match retain_within_budget(body, budget, operation, lifecycle_script).await {
        Ok(bytes) => bytes,
        Err(failure) => {
            return Err(Box::new(Refused {
                failure,
                method: parts.method,
                uri: parts.uri,
            }));
        }
    };

    Ok(Request::from_hyper(
        parts, body_bytes, origin, method, permit,
    ))
}

/// Retain one admitted body's data frames under every bound its operation
/// carries.
///
/// One turn of this loop weighs every inbound source together — the shutdown
/// and cancellation authority the envelope carries, the two request deadlines,
/// the peer's own response lifetime, and whatever the wire produced — and the
/// declared precedence then names the single terminal. Two sources that became
/// ready in the same turn are resolved by that contract rather than by which
/// future the executor happened to poll first, and nothing is polled after a
/// terminal is selected.
///
/// Every frame is accounted through the shared bound first and appended second,
/// so a frame that would carry this request past its limit is dropped while it
/// is still only a value this loop holds — the buffer never grows past what the
/// route admitted.
///
/// What is reported is the buffer's own length, read after the append. The
/// bound's running sum only ever names a total it already admitted, so a case
/// asking what this request held would be shown the arithmetic restating itself
/// rather than the bytes.
async fn retain_within_budget(
    mut body: hyper::body::Incoming,
    budget: BodyBudget,
    operation: &OperationEnvelope,
    script: Option<&super::mock::LifecycleScript>,
) -> Result<bytes::Bytes, InboundFailure> {
    operation.observe(script, OperationStage::Body);
    let mut guard = operation.inbound();
    let mut budget = budget;
    let mut retained = bytes::BytesMut::new();
    let mut read = InboundRead {
        budget: &mut budget,
        retained: &mut retained,
        guard: &mut guard,
        script,
    };
    loop {
        super::mock::LifecycleScript::pause_at(
            script,
            super::mock::LifecycleCheckpoint::BeforeInboundTerminalSelection,
        )
        .await;
        let (ready, wire) = advance(&mut body, &mut read).await;
        let Some(terminal) = ready.select() else {
            continue;
        };
        super::mock::LifecycleScript::pause_at(
            script,
            super::mock::LifecycleCheckpoint::InboundTerminalSelected(terminal),
        )
        .await;
        return match terminal {
            InboundTerminal::ResponseHead => Ok(std::mem::take(read.retained).freeze()),
            ended => Err(InboundFailure::of(ended, operation.budget(), wire)),
        };
    }
}

/// The state one inbound turn reads and writes.
///
/// Bundled because the turn is one decision over all of it: the bound that
/// admits a frame, the buffer that keeps it, the authority that can end the
/// read, and the observer that records what happened move together or not at
/// all.
struct InboundRead<'a> {
    budget: &'a mut BodyBudget,
    retained: &'a mut bytes::BytesMut,
    guard: &'a mut InboundGuard,
    script: Option<&'a super::mock::LifecycleScript>,
}

/// Advance one inbound turn, and re-collect every source it ended with.
///
/// The carried sources are read again after the wait rather than before it: a
/// deadline that expired while a frame was in flight belongs to the same turn
/// as the frame, and weighing it against a stale reading would let poll order
/// decide what the precedence table already decides.
async fn advance(
    body: &mut hyper::body::Incoming,
    read: &mut InboundRead<'_>,
) -> (InboundReady, Option<Rejected>) {
    use http_body_util::BodyExt;
    // The wire is offered first, so a frame already waiting is read into the
    // same turn as the deadlines beside it. Deciding on the deadlines alone
    // would let a body that already crossed its route's byte maximum be
    // reported as an idle expiry, which the declared precedence ranks below it.
    let frame = tokio::select! {
        biased;
        frame = body.frame() => Some(frame),
        () = read.guard.quiet() => None,
    };
    let carried = read.guard.observed();
    match frame {
        None => (carried, None),
        Some(frame) => fold(frame, read, carried),
    }
}

/// Fold one wire read into the turn's ready set.
///
/// The three wire outcomes stay apart. A limit is the bound's own answer, a
/// transport fault is the wire's, and the payload's end is neither; collapsing
/// them told a peer to shrink a request that was the right size.
fn fold(
    frame: Option<Result<hyper::body::Frame<bytes::Bytes>, hyper::Error>>,
    read: &mut InboundRead<'_>,
    carried: InboundReady,
) -> (InboundReady, Option<Rejected>) {
    let frame = match frame {
        None => return (carried.with_response_head(), None),
        Some(Ok(frame)) => frame,
        Some(Err(error)) => {
            return (
                carried.with_source_failure(),
                Some(Rejected::body_unreadable(Box::new(error))),
            );
        }
    };
    // Trailers carry no payload, so they are neither counted nor measured, and
    // they renew no quiet interval.
    let Ok(data) = frame.into_data() else {
        return (carried, None);
    };
    super::mock::LifecycleScript::count_body_frame(read.script);
    match read.budget.admit_frame(data.len()) {
        Err(rejected) => (carried.with_route_body_limit(), Some(rejected)),
        Ok(_) => {
            read.retained.extend_from_slice(&data);
            super::mock::LifecycleScript::observe_body_retained(read.script, read.retained.len());
            read.guard.frame_delivered(data.len());
            (carried, None)
        }
    }
}

/// What dispatch is given once the head has decided how to read the wire.
///
/// The WS build carries the extracted upgrade beside the request; every other
/// build has no upgrade to carry. One alias, so the paths that produce it and
/// the entry point that consumes it state the difference once.
#[cfg(feature = "ws")]
type DispatchInput = (Request, WsUpgrade);
#[cfg(not(feature = "ws"))]
type DispatchInput = Request;

/// Build a Request from head metadata with an empty body (WS-extracted).
///
/// Used for head-only routes (WebSocket, SSE) that skip body collection.
#[cfg(feature = "ws")]
fn build_head_only_request(
    mut hyper_req: hyper::Request<hyper::body::Incoming>,
    origin: RequestOrigin<'_>,
) -> DispatchInput {
    let ws = ws_proxy::extract_ws_upgrade(&mut hyper_req);
    let head = RequestHead::from_hyper_request(&hyper_req, origin);
    (head.to_request(None), ws)
}

/// Build a Request from head metadata with an empty body.
///
/// Used for head-only routes (SSE) that skip body collection.
#[cfg(not(feature = "ws"))]
fn build_head_only_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    origin: RequestOrigin<'_>,
) -> DispatchInput {
    RequestHead::from_hyper_request(&hyper_req, origin).to_request(None)
}

/// Consume a hyper request into a Camber Request (body-limited, WS-extracted).
#[cfg(feature = "ws")]
async fn collect_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    buffered: BufferedRead,
    operation: &OperationEnvelope,
    origin: RequestOrigin<'_>,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
) -> Building {
    let mut r = hyper_req;
    let ws_upgrade = ws_proxy::extract_ws_upgrade(&mut r);
    let req = collect_body_limited(r, buffered, operation, origin, lifecycle_script).await?;
    Ok((req, ws_upgrade))
}

/// Consume a hyper request into a Camber Request (body-limited).
#[cfg(not(feature = "ws"))]
async fn collect_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    buffered: BufferedRead,
    operation: &OperationEnvelope,
    origin: RequestOrigin<'_>,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
) -> Building {
    collect_body_limited(hyper_req, buffered, operation, origin, lifecycle_script).await
}

/// How the wire is read for one classified request.
///
/// Head-only routes and routing terminals never expose a body to a handler, so
/// collecting one would buffer bytes no handler can reach. Only a matched
/// buffered route carries a read plan, because only that arm reads a body, and
/// the plan is what its admission already settled.
enum WireRead {
    /// Build from the head alone and drop the incoming body.
    HeadOnly,
    /// Collect the body under the admission this route's plan resolved to.
    Body(BufferedRead),
}

/// What one admitted buffered route reads, and under what.
///
/// The bound and the permit arrive together from admission, which ran before
/// this request's first payload frame. The permit moves into the owned
/// `Request` collection builds, so the decision and the ownership it granted
/// stay one value all the way through.
struct BufferedRead {
    method: super::method::Method,
    budget: BodyBudget,
    permit: Option<BodyPermit>,
}

/// Read the wire the way the head classification decided it should be read.
async fn build_dispatch_input(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    origin: RequestOrigin<'_>,
    lifecycle: &ConnectionLifecycle,
    operation: &OperationEnvelope,
    read: WireRead,
) -> Building {
    match read {
        WireRead::HeadOnly => Ok(build_head_only_request(hyper_req, origin)),
        WireRead::Body(buffered) => {
            let lifecycle_script = lifecycle.script();
            collect_request(
                hyper_req,
                buffered,
                operation,
                origin,
                lifecycle_script.as_deref(),
            )
            .await
        }
    }
}

/// What the request head decides before a body is read.
///
/// The two variants differ in size because a classified route carries the
/// budgets its layers resolved, which are validated `Copy` values. Boxing that
/// variant would put a heap allocation on every routed request to shrink a
/// value that is built, matched, and consumed inside one frame — and the
/// internal-route variant, which the difference is measured against, is
/// reachable only for the three built-in paths.
#[expect(
    clippy::large_enum_variant,
    reason = "one stack-local value per request; boxing would allocate to save nothing"
)]
enum PreBodyRoute<'a> {
    /// An internal route (`/health`, `/metrics`, `/debug/pprof/cpu`), which
    /// bypasses body collection entirely.
    Internal(super::internal_routes::InternalRoute),
    /// How the dispatch trie classifies the route. A class that reads a body
    /// carries the method it matched under, because that is the one arm that
    /// needs it and re-parsing it would only re-derive what the head proved.
    Class(Classified<'a>),
}

/// Classify a request from its head alone.
///
/// The borrow of `hyper_req` ends here, before any mutable access or body
/// collection.
///
/// The upgrade question is answered here, where the hyper headers are still in
/// hand, and carried in: a streaming-proxy route that carries one is dispatched
/// as a handshake, so classification has to know before it decides what to
/// build.
fn classify_pre_body<'a>(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    dispatch: &'a ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
) -> PreBodyRoute<'a> {
    let head = RequestHead::from_hyper_request(hyper_req, origin);
    let internal = match_internal_route_from_path(head.path(), ctx);
    #[cfg(feature = "profiling")]
    let internal = internal.or_else(|| match_profiling_route(head.path(), head.raw_query(), ctx));
    #[cfg(feature = "ws")]
    let asks_websocket = ws_proxy::is_ws_upgrade_head(hyper_req.headers());
    // No WebSocket support is compiled in, so no head can ask for one.
    #[cfg(not(feature = "ws"))]
    let asks_websocket = false;
    match internal {
        Some(route) => PreBodyRoute::Internal(route),
        None => PreBodyRoute::Class(dispatch.classify_route(
            &head,
            HeadUpgrade::of(asks_websocket),
            ctx.policy,
        )),
    }
}

/// Build the middleware gate a dispatch result still owes.
///
/// Synchronous by necessity: `DispatchResult` is not `Sync`, so the borrow it
/// takes must not be held across an await. The returned check owns everything
/// it needs.
fn pending_middleware_gate(
    result: &DispatchResult,
    router: Option<&FrozenRouter>,
    scope: &RejectionScope,
) -> Option<GateCheck> {
    match (result.needs_middleware_gate(), router) {
        (false, _) => None,
        (true, Some(router)) => router.middleware_gate(result.request_ref(), scope),
        // Named rather than folded into the pass above. Only a resolved router
        // has handlers to select a gated class from, so nothing reaches here
        // today — but the pass arm forwards, and a stream, an SSE, or a proxied
        // upgrade forwarded on a gate that never ran is an unauthenticated
        // request. Refused instead, which is the answer that stays safe if the
        // shape ever changes.
        (true, None) => Some(unrunnable_gate(scope)),
    }
}

/// Answer a gate that was owed but had no chain to run it.
fn unrunnable_gate(scope: &RejectionScope) -> GateCheck {
    let scope = scope.clone();
    Box::pin(async move { scope.map(Rejected::gate_unrunnable()) })
}

/// Put a buffered response on the wire and record the status the peer is given.
///
/// The record is read off the CONVERTED response, never off the one handed in.
/// A response Camber cannot build leaves through the rejection boundary with
/// its own status — an unrepresentable header name is enough, and `with_header`
/// does not validate — so a metric taken before the conversion could name a
/// status no peer ever saw.
///
/// The scope owns the conversion, the HEAD body strip, and the method and path
/// this request is recorded under: every buffered answer this file gives leaves
/// through here, so none of them can strip a body another one keeps or record a
/// name another one uses.
///
/// A response policy produced is also counted as the refusal it answered.
/// Conversion reports that category alongside the response, because conversion
/// is the last stage that can raise one of its own.
pub(super) fn answer(
    ctx: &ConnCtx,
    resp: Response,
    start: std::time::Instant,
    scope: &RejectionScope,
) -> hyper::Response<HyperResponseBody> {
    let finalized = scope.finalize(resp);
    let status = finalized.response.status().as_u16();
    if let Some(kind) = finalized.refused {
        count_rejection(ctx, kind, status);
    }
    record_scoped(ctx, scope, status, start);
    let (parts, body) = finalized.response.into_parts();
    hyper::Response::from_parts(parts, HyperResponseBody::Full(body))
}

/// Answer one refusal under the policy its own stage resolved.
///
/// Every mapped refusal this file answers with leaves through here, so none of
/// them can map under one policy and record under another.
pub(super) fn answer_rejected(
    ctx: &ConnCtx,
    scope: &RejectionScope,
    rejected: Rejected,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let response = scope.map(rejected);
    answer(ctx, response, start, scope)
}

/// The per-connection facts every classified head shares, before an operation
/// exists.
///
/// Held apart from [`RequestDispatch`] because the difference is the contract:
/// a head answered from here has no envelope, and a head answered from there
/// always has exactly one.
pub(super) struct ConnDispatch<'a> {
    dispatch: &'a ServerDispatch,
    ctx: &'a ConnCtx,
    origin: RequestOrigin<'a>,
    lifecycle: &'a ConnectionLifecycle,
    connection: &'a Arc<ConnectionLiveness>,
    start: std::time::Instant,
}

impl<'a> ConnDispatch<'a> {
    /// This connection's facts, under the one envelope an admitted head minted.
    fn admitted(&self, operation: &'a OperationEnvelope) -> RequestDispatch<'a> {
        RequestDispatch {
            dispatch: self.dispatch,
            ctx: self.ctx,
            origin: self.origin,
            lifecycle: self.lifecycle,
            start: self.start,
            operation,
        }
    }
}

/// Shared facts needed after a request head has selected a dispatch route.
pub(super) struct RequestDispatch<'a> {
    dispatch: &'a ServerDispatch,
    pub(super) ctx: &'a ConnCtx,
    pub(super) origin: RequestOrigin<'a>,
    pub(super) lifecycle: &'a ConnectionLifecycle,
    pub(super) start: std::time::Instant,
    /// The one envelope this admitted head minted.
    ///
    /// Borrowed rather than owned, because the envelope is not cloneable and
    /// this value is read by every owner the request reaches. A head that
    /// resolved no route authority never reaches here: it is answered before an
    /// operation exists.
    pub(super) operation: &'a OperationEnvelope,
}

/// How the wire must be read for a class that reaches dispatch.
///
/// Listed rather than wildcarded, so a new `RouteClass` is a compile error here
/// instead of a silent fall-through into body collection. `StreamingProxy` and
/// `StreamingMultipart` are listed for exhaustiveness alone: the caller answers
/// both before this runs — one forwards its own body, the other opens its own
/// session over it — so neither reaches here.
async fn wire_read(
    route_class: RouteClass,
    origin: RequestOrigin<'_>,
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    lifecycle: &ConnectionLifecycle,
) -> Result<WireRead, Rejected> {
    match route_class {
        RouteClass::Buffered { method, plan } => {
            admitted_read(method, &plan, origin, hyper_req, lifecycle).await
        }
        RouteClass::HeadOnly
        | RouteClass::Terminal
        | RouteClass::Refused(_)
        | RouteClass::StreamingProxy(_)
        | RouteClass::StreamingMultipart(_) => Ok(WireRead::HeadOnly),
    }
}

/// Admit one buffered route's body before its first frame is polled.
///
/// The head is borrowed here and nowhere else: admission answers from what the
/// request head established, and the borrow ends before the body is consumed.
/// Everything past that call is packing: the decision, its reported limit, and
/// its permit all come from the one admission both body modes enter through.
async fn admitted_read(
    method: super::method::Method,
    plan: &ResolvedBodyPlan,
    origin: RequestOrigin<'_>,
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    lifecycle: &ConnectionLifecycle,
) -> Result<WireRead, Rejected> {
    let head = RequestHead::from_hyper_request(hyper_req, origin);
    let script = lifecycle.script();
    let admitted = super::body_admission::admit(plan, &head, script.as_ref()).await?;
    let budget = BodyBudget::new(&admitted);
    Ok(WireRead::Body(BufferedRead {
        method,
        budget,
        permit: admitted.permit,
    }))
}

/// Answer a classified route, minting the one operation an admitted head owns.
///
/// The refusal arm answers first, and it answers before any envelope exists: a
/// head that resolved no route authority has no policy to run under, so there is
/// nothing for an operation to carry and no identity for one to be named by.
async fn dispatch_classified_route<'a>(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    classified: Classified<'a>,
    conn: &'a ConnDispatch<'a>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &ConnDispatch {
        ctx,
        origin,
        lifecycle,
        connection,
        start,
        ..
    } = conn;

    let Classified {
        class,
        scope,
        router,
        budgets,
    } = classified;
    // Reported where routing resolved it, for the same reason the admitted body
    // limit is: the number an operator or a test reads is the one the routing
    // owner decided, not one a later consumer re-derived.
    let script = lifecycle.script();
    super::mock::LifecycleScript::pause_at(
        script.as_deref(),
        super::mock::LifecycleCheckpoint::RouteBudgetsResolved {
            request: budgets.request(),
            upload: budgets.upload(),
            download: budgets.download(),
        },
    )
    .await;
    let class = match class {
        RouteClass::Refused(rejected) => {
            return Ok(refuse_from_head(
                ctx, origin, &scope, &hyper_req, rejected, start,
            ));
        }
        admitted @ (RouteClass::StreamingProxy(_)
        | RouteClass::StreamingMultipart(_)
        | RouteClass::HeadOnly
        | RouteClass::Terminal
        | RouteClass::Buffered { .. }) => admitted,
    };
    // One envelope per admitted head, minted from the policy the classifier
    // just resolved and carried from here to the response-head boundary.
    let operation = OperationEnvelope::admit(
        budgets,
        lifecycle.control(),
        connection,
        origin.disconnect,
        ctx.policy.shutdown_timeout_value(),
        script.as_deref(),
    );
    operation.observe(script.as_deref(), OperationStage::Dispatch);
    let request_dispatch = conn.admitted(&operation);
    dispatch_admitted_route(hyper_req, class, router, &scope, &request_dispatch).await
}

/// Answer one admitted class under the operation its head already minted.
///
/// The wire contract is asked for only by the classes that go on to read the
/// wire. The two arms below answer without reading one, so deriving it for them
/// was work every proxied stream paid to drop.
async fn dispatch_admitted_route<'a>(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    class: RouteClass,
    router: Option<&'a FrozenRouter>,
    scope: &PreBodyScope,
    request_dispatch: &RequestDispatch<'a>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &RequestDispatch {
        ctx,
        origin,
        lifecycle,
        start,
        operation,
        ..
    } = request_dispatch;
    let class = match class {
        RouteClass::StreamingProxy(target) => {
            return dispatch_streaming_proxy(hyper_req, target, router, scope, request_dispatch)
                .await;
        }
        RouteClass::StreamingMultipart(target) => {
            return super::multipart_route::dispatch_streaming_multipart(
                hyper_req,
                target,
                router,
                scope,
                request_dispatch,
            )
            .await;
        }
        read_from_wire @ (RouteClass::HeadOnly
        | RouteClass::Terminal
        | RouteClass::Buffered { .. }) => read_from_wire,
        // Answered before this function was reached, and named rather than
        // wildcarded so a class that gains a refusal cannot fall through into
        // body collection.
        RouteClass::Refused(rejected) => {
            return Ok(refuse_from_head(
                ctx, origin, scope, &hyper_req, rejected, start,
            ));
        }
    };

    let read = match wire_read(class, origin, &hyper_req, lifecycle).await {
        Ok(read) => read,
        Err(rejected) => {
            return Ok(refuse_from_head(
                ctx, origin, scope, &hyper_req, rejected, start,
            ));
        }
    };
    let input = match build_dispatch_input(hyper_req, origin, lifecycle, operation, read).await {
        Ok(input) => input,
        Err(refused) => {
            return Ok(refuse_body(ctx, origin, scope, *refused, start));
        }
    };
    dispatch_built_request(input, router, request_dispatch).await
}

/// Answer a refusal decided from the request head alone.
///
/// Both producers reach here — the head classification's own refusal and the
/// admission decision that precedes the first body poll — so neither can map
/// under one policy while the other maps under a rebuilt one.
fn refuse_from_head(
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    pre_body: &PreBodyScope,
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    rejected: Rejected,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let scope = pre_body.scope(RequestIdentity::from_head(
        &origin,
        hyper_req.method(),
        hyper_req.uri(),
    ));
    answer_rejected(ctx, &scope, rejected, start)
}

/// Answer a refusal raised while the request body was being read.
///
/// No owned request exists yet, so the policy and the route identity come from
/// the classification that decided how the wire would be read. That is the same
/// mapper the route's own handler would have failed through.
fn refuse_body(
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    pre_body: &PreBodyScope,
    refused: Refused,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let Refused {
        failure,
        method,
        uri,
    } = refused;
    let scope = pre_body.scope(RequestIdentity::from_head(&origin, &method, &uri));
    answer_inbound_failure(ctx, &scope, failure, start)
}

/// Answer one selected inbound terminal with the disposition it declares.
///
/// Both owners that select one reach here — the buffered read loop and the
/// streaming multipart session — so a mapped cause cannot be answered once
/// through this route's mapper and once through a second spelling of the same
/// two arms.
pub(super) fn answer_inbound_failure(
    ctx: &ConnCtx,
    scope: &RejectionScope,
    failure: InboundFailure,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    match failure {
        InboundFailure::Mapped(rejected) => answer_rejected(ctx, scope, rejected, start),
        InboundFailure::Silent(terminal) => end_without_mapping(ctx, scope, terminal, start),
    }
}

/// End one admitted request that the declared precedence gives no mapper.
///
/// The shutdown deadline, an explicit cancellation, and a peer whose response
/// lifetime already ended stop the work rather than refusing it. There is no
/// refusal for a route's mapper to shape and, on two of the three rows, no peer
/// left to read one. The transport closes, the status an operator sees is
/// recorded under the same scope every other exit records under, and no
/// rejection category is counted for a request nothing categorised.
pub(super) fn end_without_mapping(
    ctx: &ConnCtx,
    scope: &RejectionScope,
    terminal: InboundTerminal,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let status = match terminal {
        InboundTerminal::Disconnect => hyper::StatusCode::REQUEST_TIMEOUT,
        InboundTerminal::ShutdownDeadline
        | InboundTerminal::ForcedCancellation
        | InboundTerminal::RouteBodyLimit
        | InboundTerminal::TransferBytes
        | InboundTerminal::BodyIdle
        | InboundTerminal::TransferIdle
        | InboundTerminal::TransferTotal
        | InboundTerminal::RequestTotal
        | InboundTerminal::SourceFailure
        | InboundTerminal::ResponseHead => hyper::StatusCode::SERVICE_UNAVAILABLE,
    };
    record_scoped(ctx, scope, status.as_u16(), start);
    let mut response = hyper::Response::new(HyperResponseBody::Full(http_body_util::Full::new(
        bytes::Bytes::new(),
    )));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONNECTION,
        hyper::header::HeaderValue::from_static("close"),
    );
    response
}

/// Run middleware gates and finish a request whose wire representation is built.
///
/// The child router arrives from classification rather than being resolved
/// again: the authority parse and the host-table search that selected it are
/// the same two this dispatch would otherwise repeat.
async fn dispatch_built_request<'a>(
    input: DispatchInput,
    resolved: Option<&'a FrozenRouter>,
    request_dispatch: &RequestDispatch<'a>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &RequestDispatch {
        dispatch,
        ctx,
        lifecycle,
        start,
        operation,
        ..
    } = request_dispatch;
    let script = lifecycle.script();
    #[cfg(feature = "ws")]
    let (req, ws_upgrade) = input;
    #[cfg(not(feature = "ws"))]
    let req = input;
    let Routed {
        result,
        router,
        scope,
    } = dispatch.dispatch_resolved(req, resolved);
    // A proxied route that carries an upgrade dispatches as WebSocket, not as
    // the proxy class its handler was registered under. Corrected before the
    // gate runs, so a gate refusal reports the class this request is actually
    // being answered as.
    #[cfg(feature = "ws")]
    let scope = match result.is_websocket() {
        true => scope.reclassified(RejectionProtocol::WebSocket),
        false => scope,
    };
    // Built in its own statement: `DispatchResult` is not `Sync`, so the borrow
    // this takes must end before the check is awaited.
    let gate = pending_middleware_gate(&result, router, &scope);
    operation.observe(script.as_deref(), OperationStage::Middleware);
    // Refused as it stands: this class declared no payload it leaves unread.
    let answering = gate_within_total(gate_outcome(gate), request_dispatch, &scope, identity);
    if let Some(answered) = answering.await {
        return Ok(answered);
    }

    #[cfg(feature = "ws")]
    if let Some(rejected) = result
        .is_websocket()
        .then(|| ws_proxy::check_ws_origin(result.request_ref()))
        .flatten()
    {
        return Ok(answer_rejected(ctx, &scope, rejected, start));
    }

    operation.observe(script.as_deref(), OperationStage::ResponseHead);
    finish_dispatched(
        result,
        #[cfg(feature = "ws")]
        ws_upgrade,
        &scope,
        request_dispatch,
    )
    .await
}

/// Answer one dispatched result under the scope and operation it resolved to.
///
/// Each arm names the owner that finishes its class, and the two streaming arms
/// resolve their download owner here rather than inside the body: the head this
/// function commits is where a download's own lifetime begins, and a body handed
/// no owner would have no policy left to read.
async fn finish_dispatched<'a>(
    result: DispatchResult,
    #[cfg(feature = "ws")] ws_upgrade: WsUpgrade,
    scope: &RejectionScope,
    request_dispatch: &RequestDispatch<'a>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &RequestDispatch {
        ctx,
        lifecycle,
        start,
        operation,
        ..
    } = request_dispatch;
    let script = lifecycle.script();
    match result {
        DispatchResult::Async(fut, held_request) => {
            finish_buffered(fut, held_request, ctx, scope, start, operation).await
        }
        DispatchResult::Stream(fut, req) => {
            finish_stream(fut, req, ctx, scope, start, operation, lifecycle).await
        }
        DispatchResult::Sse(handler, req) => {
            record_scoped(ctx, scope, 200, start);
            handle_sse(handler, req, ctx.sse_buffer_size, lifecycle, operation).await
        }
        #[cfg(feature = "ws")]
        DispatchResult::WebSocket(handler, req) => {
            record_upgrade(ctx, req, start, scope, operation, |req| {
                ws_proxy::handle_ws_upgrade(ws_upgrade, handler, req, ctx.ws_buffer_size, lifecycle)
            })
            .await
        }
        #[cfg(feature = "ws")]
        DispatchResult::ProxyWebSocket(req, backend, prefix) => {
            record_upgrade(ctx, req, start, scope, operation, |req| {
                ws_proxy::handle_proxy_ws(ws_upgrade, req, backend, prefix, lifecycle)
            })
            .await
        }
        DispatchResult::ProxyStream(req, backend, prefix) => {
            let download = operation.download(super::TransferBudget::unbounded(), script);
            handle_proxy_stream_response(req, &backend, &prefix, ctx, scope, start, download).await
        }
    }
}

/// Produce one response head inside the request total its operation carries.
///
/// The total ends at the committed head, so this is the last owner that can
/// enforce it: response-body time belongs to the selected download budget, not
/// to the request. An unbounded total awaits the producer directly rather than
/// arming a timer no configured value backs.
pub(super) async fn within_total<T>(
    producing: impl std::future::Future<Output = T>,
    operation: &OperationEnvelope,
) -> Result<T, Rejected> {
    let Some(remaining) = operation.remaining_total() else {
        return Ok(producing.await);
    };
    tokio::time::timeout(remaining, producing)
        .await
        .map_err(|_| Rejected::request_timeout(operation.budget().total().unwrap_or(remaining)))
}

/// Run one middleware gate inside the request total its operation carries.
///
/// Every class that owes a gate reaches here. The chain is admitted work before
/// the committed head, exactly like the producer behind it, and a buffered
/// route's chain is already bounded because dispatch builds it into the future
/// [`finish_buffered`] wraps. A gate awaited outside the total left the same
/// middleware refused at the total on `router.get` and unbounded on
/// `router.get_stream`.
///
/// `Some` is the answer that finishes this request: the chain's own refusal, or
/// the total's when the chain outlives it. `None` is a pass. What a refusal owes
/// the transport is the caller's to say through `refuse` — a class that declared
/// a payload it never read has to close the connection the next request would
/// otherwise be framed out of, and a class that declared none does not.
pub(super) async fn gate_within_total(
    gate: impl std::future::Future<Output = Option<Response>>,
    request_dispatch: &RequestDispatch<'_>,
    scope: &RejectionScope,
    refuse: impl FnOnce(Response) -> Response,
) -> Option<hyper::Response<HyperResponseBody>> {
    let &RequestDispatch {
        ctx,
        start,
        operation,
        ..
    } = request_dispatch;
    match within_total(gate, operation).await {
        Ok(None) => None,
        Ok(Some(blocked)) => Some(answer(ctx, refuse(blocked), start, scope)),
        Err(rejected) => Some(answer_rejected(ctx, scope, rejected, start)),
    }
}

/// Route a request and dispatch to the appropriate handler.
///
/// The request clock starts here, as the first thing this function does. It is
/// the only clock every route class shares: starting one per class put three
/// meanings of "request duration" into one histogram, and starting the buffered
/// one after the body was read left out the inbound-body time a slow or large
/// upload is made of.
pub(super) async fn handle_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    remote_addr: Option<std::net::IpAddr>,
    lifecycle: &ConnectionLifecycle,
    lifetime: ResponseLifetime,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let start = std::time::Instant::now();
    // Built once and copied down every dispatch path, so no path can pair this
    // peer with another request's lifetime signal. The identity is minted here,
    // after Hyper accepted the head and before method, host, route, body,
    // middleware, or protocol classification has run, so every answer this
    // request can get names the same value.
    let origin = RequestOrigin {
        remote_addr,
        is_tls: ctx.is_tls,
        request_id: RequestId::generate(),
        version: hyper_req.version(),
        disconnect: lifetime.signal(),
    };

    // gRPC bodies are streaming — skip body collection and dispatch directly to tonic.
    // Middleware runs as a gate check on the headers, then forwards to tonic.
    #[cfg(feature = "grpc")]
    let hyper_req = match try_dispatch_grpc(hyper_req, dispatch, ctx, origin, start).await {
        GrpcDispatch::Handled(resp) => return resp,
        GrpcDispatch::NotGrpc(req) => req,
    };

    let classified = match classify_pre_body(&hyper_req, dispatch, ctx, origin) {
        // Internal routes (/health, /metrics, /debug/pprof/cpu) bypass body
        // collection.
        PreBodyRoute::Internal(route) => {
            return dispatch_internal_head_only(&hyper_req, route, dispatch, ctx, origin, start)
                .await;
        }
        PreBodyRoute::Class(classified) => classified,
    };
    let conn = ConnDispatch {
        dispatch,
        ctx,
        origin,
        lifecycle,
        connection: lifetime.connection(),
        start,
    };
    dispatch_classified_route(hyper_req, classified, &conn).await
}

/// Produce one buffered answer inside the request total its operation carries.
///
/// The total covers handler execution and response production and ends at the
/// committed head, so a handler that never returns holds this operation no
/// longer than the policy its route resolved to allows.
async fn finish_buffered(
    producing: super::middleware::ResponseFuture,
    held_request: Request,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    start: std::time::Instant,
    operation: &OperationEnvelope,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let answered = match within_total(producing, operation).await {
        Ok(response) => finish_async(ctx, response, start, scope),
        Err(rejected) => Ok(answer_rejected(ctx, scope, rejected, start)),
    };
    // Released here, not at the start of this arm: the request owns this
    // response's lifetime signal, and the buffered future is `'static`, so
    // nothing else keeps that signal alive while the handler runs.
    drop(held_request);
    answered
}

/// Produce one streaming handler's head inside the request total it carries.
///
/// A streaming route's head is produced by the same user code a buffered one
/// runs — the difference is only what the future resolves to — so it spends the
/// same total. The body behind the committed head does not: it belongs to the
/// download budget, and [`handle_stream_response`] runs after this returns.
async fn finish_stream(
    producing: std::pin::Pin<
        Box<dyn std::future::Future<Output = super::stream::StreamResponse> + Send>,
    >,
    held_request: Request,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    start: std::time::Instant,
    operation: &OperationEnvelope,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    match within_total(producing, operation).await {
        Ok(stream_resp) => {
            // The response the handler produced names its own policy, and the
            // route's contains it: resolved here, at the one head this arm
            // commits, rather than inside the body that then has no policy to
            // read.
            let download = operation.download(stream_resp.budget(), lifecycle.script());
            handle_stream_response(stream_resp, held_request, ctx, scope, start, download)
        }
        Err(rejected) => {
            let answered = Ok(answer_rejected(ctx, scope, rejected, start));
            // Released here for the same reason the buffered arm releases it
            // last: the request owns this response's lifetime signal, and a
            // handler dropped by the expiring total must not be told the
            // request ended before its refusal is built.
            drop(held_request);
            answered
        }
    }
}

/// Finish a buffered dispatch against the request that produced it.
///
/// Both buffered entry points end here, and here ends in [`answer`], so neither
/// can strip a body the other keeps or record a status it did not answer with.
fn finish_async(
    ctx: &ConnCtx,
    resp: Response,
    start: std::time::Instant,
    scope: &RejectionScope,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    Ok(answer(ctx, resp, start, scope))
}

/// Run an upgrade and record what it ANSWERED, not the `101` it hoped for.
///
/// A rejected handshake, a refused registrar, or an unbuildable `101` leaves as
/// a refusal, mapped here by the same policy the route's own handler would have
/// failed through — named by what negotiation had selected before it failed.
/// Both upgrade kinds record through here, so that invariant is stated once
/// rather than per kind.
///
/// The name comes off the scope, not off the request that moves into the
/// bridge. Rebuilding it here from the request's own method and URI was a
/// second answer to a question every exit already carries one for, and the
/// buffered and streaming exits cannot disagree about what names a request
/// while they read the same one.
///
/// The handshake runs inside the request total, which ends at the handoff this
/// records. Everything up to it is still the request — the negotiation, and for
/// a proxied upgrade the dial and handshake of the upstream leg — so an upgrade
/// whose peer or upstream never completes it is refused on the same deadline
/// every other admitted route answers to. The session past a committed `101`
/// spends no request time: it is bounded by the WebSocket quotas its own
/// registration carries.
#[cfg(feature = "ws")]
async fn record_upgrade<F, Fut>(
    ctx: &ConnCtx,
    req: Request,
    start: std::time::Instant,
    scope: &RejectionScope,
    operation: &OperationEnvelope,
    upgrade: F,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible>
where
    F: FnOnce(Request) -> Fut,
    Fut: std::future::Future<
            Output = Result<hyper::Response<HyperResponseBody>, ws_proxy::WsRefusal>,
        >,
{
    let handed_off = match within_total(upgrade(req), operation).await {
        Ok(handed_off) => handed_off,
        Err(rejected) => return Ok(answer_rejected(ctx, scope, rejected, start)),
    };
    match handed_off {
        Ok(resp) => {
            record_scoped(ctx, scope, resp.status().as_u16(), start);
            Ok(resp)
        }
        Err(refusal) => Ok(answer_rejected(
            ctx,
            &refused_upgrade_scope(scope, refusal.subprotocol.as_deref()),
            refusal.rejected,
            start,
        )),
    }
}

/// Name a refused upgrade's policy by what negotiation had already selected.
#[cfg(feature = "ws")]
fn refused_upgrade_scope(scope: &RejectionScope, subprotocol: Option<&str>) -> RejectionScope {
    match subprotocol {
        Some(subprotocol) => scope.clone().negotiated_subprotocol(subprotocol),
        None => scope.clone(),
    }
}

/// Dispatch an internal route without body collection.
///
/// Internal routes (/health, /metrics, /debug/pprof/cpu) never need the
/// request body. This function builds a lightweight Request from head
/// metadata when middleware requires it, or invokes directly when middleware
/// is bypassed.
async fn dispatch_internal_head_only(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    route: super::internal_routes::InternalRoute,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    match dispatch.skip_middleware_for_internal() {
        true => Ok(answer_internal_directly(hyper_req, &route, dispatch, ctx, origin, start).await),
        false => {
            dispatch_internal_through_middleware(hyper_req, route, dispatch, ctx, origin, start)
                .await
        }
    }
}

/// Answer an internal route that bypasses the middleware chain.
///
/// The bypass skips middleware, not policy: a route Camber could not build a
/// response for is mapped by the same mapper every other refusal reaches.
///
/// Async because the route it invokes is: the profiling route waits on a
/// blocking thread, and a bypass that resolved it synchronously would hold this
/// worker for the whole sampling window.
async fn answer_internal_directly(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    route: &super::internal_routes::InternalRoute,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let head = RequestHead::from_hyper_request(hyper_req, origin);
    let identity = RequestIdentity::from_head(&origin, hyper_req.method(), hyper_req.uri());
    let scope = internal_scope(dispatch.head_scope(&head, identity), route);
    let response = scope.resolve(invoke_internal_route(route).await, HANDLER);
    answer(ctx, response, start, &scope)
}

/// Name one internal route's policy by the fixed identity it dispatches under.
fn internal_scope(
    scope: RejectionScope,
    route: &super::internal_routes::InternalRoute,
) -> RejectionScope {
    scope.established(route.route(), RejectionProtocol::OrdinaryHttp)
}

/// Build a lightweight Request from head metadata and run the internal route
/// through the middleware chain.
///
/// The child router is resolved once and handed to both the scope and the
/// dispatch. `/health` and `/metrics` are the highest-frequency paths a served
/// process sees, and asking each of them separately cost two authority parses
/// and two host-table searches per request.
async fn dispatch_internal_through_middleware(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    route: super::internal_routes::InternalRoute,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let req = RequestHead::from_hyper_request(hyper_req, origin).to_request(None);
    let resolved = dispatch.resolve(&req);
    let scope = internal_scope(dispatch.resolved_scope(&resolved, &req), &route);
    let handler = build_internal_handler(route);
    let AsyncDispatch {
        fut,
        req: held_request,
    } = ServerDispatch::dispatch_with_handler(resolved, &handler, req, scope.clone());
    let answered = finish_async(ctx, fut.await, start, &scope);
    // Released here, not before the await: the request owns this response's
    // lifetime signal, and the buffered future does not borrow from it.
    drop(held_request);
    answered
}

/// What the gRPC pre-check decided about a request.
///
/// Two outcomes, not a success and a failure: a request that is not gRPC has
/// nothing wrong with it, so it leaves carrying the hyper request the normal
/// HTTP path still has to read.
#[cfg(feature = "grpc")]
enum GrpcDispatch {
    /// The request was gRPC, and this is the answer it was given.
    Handled(Result<hyper::Response<HyperResponseBody>, std::convert::Infallible>),
    /// The request was not gRPC, and is handed back untouched.
    NotGrpc(hyper::Request<hyper::body::Incoming>),
}

/// Dispatch a request to tonic when it is gRPC, or hand it back when it is not.
#[cfg(feature = "grpc")]
async fn try_dispatch_grpc(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> GrpcDispatch {
    // The router is resolved here and carried into the dispatch, so being gRPC
    // and having somewhere to send it is one decision. Re-asking downstream
    // would invent a "no router" state the caller has already ruled out, and
    // that state would need an answer no rejection stage produced.
    match dispatch.grpc_router() {
        Some(grpc_router) if is_grpc_request(&hyper_req) => GrpcDispatch::Handled(
            dispatch_grpc_inner(hyper_req, grpc_router, dispatch, ctx, origin, start).await,
        ),
        _ => GrpcDispatch::NotGrpc(hyper_req),
    }
}

/// Run the middleware gate and dispatch to tonic. Called only when the request
/// is confirmed gRPC and its caller has resolved the router that answers it.
#[cfg(feature = "grpc")]
async fn dispatch_grpc_inner(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    grpc_router: &super::grpc_support::GrpcRouter,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    // Build a lightweight Request from headers for the middleware gate check.
    // The streaming gRPC body is preserved for tonic.
    //
    // The pre-check selecting this class is the establishment transition: it
    // resolved the gRPC router this request dispatches to, so the class carries
    // both the identity that registration is named by and the class itself.
    // Tonic owns method routing past the handoff, so the identity is the class's
    // fixed one rather than a trie pattern.
    //
    // The identity borrows the head it names. The request outlives it here, and
    // the identity takes its own `Bytes`-backed URI handle, so a pair of locals
    // cloned only to be lent out was a copy nothing read afterwards.
    //
    // The child router is resolved once, here, and handed to both the scope and
    // the gate. Asking separately cost two authority parses and two host-table
    // searches on every gRPC request.
    let head = RequestHead::from_hyper_request(&hyper_req, origin);
    let resolved = dispatch.resolve_from_head(&head);
    let scope = dispatch
        .resolved_head_scope(
            &resolved,
            RequestIdentity::from_head(&origin, hyper_req.method(), hyper_req.uri()),
        )
        .established(super::grpc_support::grpc_route(), RejectionProtocol::Grpc);
    // An authority Camber cannot parse is refused before the gate, not read as
    // a pass: this path forwards to tonic on a pass, so the chain would never
    // have run for a request that reached the upstream.
    let router = match resolved {
        Ok(router) => router,
        Err(rejected) => return Ok(answer_rejected(ctx, &scope, rejected, start)),
    };
    match run_head_gate(&head, router, None, &scope).await {
        // Answered under the scope this class established, not a rebuilt
        // built-in one: a middleware response the wire cannot carry recovers
        // through the router's own mapper, the same one every other refusal on
        // this path reaches.
        Some(refusal) => Ok(answer(ctx, refusal, start, &scope)),
        None => {
            // Tonic owns the response body from here, so this handoff is
            // Camber's last observation of the request's lifetime.
            origin.disconnect.complete();
            let response = grpc_router.dispatch(hyper_req).await?;
            // The head is still Camber's to see, and every other dispatch class
            // records the answer it gave. Recorded under the same scope this
            // class established, so a gRPC request is countable and
            // request-id-keyed like the rest — a class that recorded nothing
            // was a class an operator could not see at all. What stays outside
            // is what tonic owns past the head: the trailer status and anything
            // the body fails with.
            record_scoped(ctx, &scope, response.status().as_u16(), start);
            Ok(response)
        }
    }
}

/// Run middleware as a gate check for a streaming request (gRPC, streaming proxy).
///
/// Borrows URI and HeaderMap from the hyper request via `RequestHead`. Only
/// clones into an owned `Request` when middleware actually exists. `Some` is
/// the answer the chain gave instead of passing the request through; `None` is
/// a pass, either because the chain passed it or because there was no chain.
///
/// The child router arrives already resolved. Both callers — the gRPC path and
/// the streaming proxy — resolved it to select the policy their refusals are
/// mapped under, and resolving it again here cost an authority parse and a
/// host-table search on every one of their requests.
///
/// An authority no router can be resolved from is still a refusal, not a pass:
/// both callers forward upstream on `None`, so an unresolvable authority read
/// as a pass would reach the upstream with the chain never run. That refusal is
/// now answered at the single resolution each caller makes, before this gate is
/// reached at all.
pub(super) async fn run_head_gate(
    head: &RequestHead<'_>,
    router: Option<&FrozenRouter>,
    params: Option<super::request::Params>,
    scope: &RejectionScope,
) -> Option<Response> {
    gate_outcome(router.and_then(|router| router.middleware_gate_head(head, params, scope))).await
}

/// What one gate check answers with, for every caller that runs one.
///
/// `None` is a pass, either because the chain passed the request through or
/// because there was no chain to run. Written once because a caller that read a
/// pass as a short-circuit would answer a request the chain admitted, and one
/// that read a short-circuit as a pass would run a handler the chain refused.
/// Which request a gate is given is each caller's own decision — the streaming
/// classes gate borrowed head metadata, the multipart class gates the very
/// request its handler receives — and this is the one part they share.
pub(super) async fn gate_outcome(gate: Option<GateCheck>) -> Option<Response> {
    match gate {
        Some(gate) => gate_result(gate.await),
        None => None,
    }
}
