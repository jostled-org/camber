use super::body_admission::{BodyPolicy, ConfiguredCeiling, RequestBodyMode, ResolvedBodyPlan};
use super::host_router::FrozenHostRouter;
use super::method::Method;
use super::middleware::{MiddlewareFn, Next, ResponseFuture, Terminal};
use super::rejection::{
    Rejected, RejectionMapper, RejectionProtocol, RejectionScope, RequestIdentity,
};
use super::request::{Params as RequestParams, RequestHead};
use super::stream::StreamResponse;
pub(super) use super::trie::Handler;
pub(super) use super::trie::SseHandler;
#[cfg(feature = "ws")]
pub(super) use super::trie::WsHandler;
use super::trie::{
    FrozenNode, MultipartRegistration, PATH_SEGMENT_LIMIT, RouteHandler, RouteLookup, Selected,
    split_path_segments,
};
use super::{Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The upstream a streaming-proxy route resolved to.
///
/// One payload rather than four loose parameters, so the dispatch call it
/// feeds carries the route as a single value.
pub(super) struct StreamingProxyTarget {
    pub(super) backend: Arc<str>,
    pub(super) prefix: Arc<str>,
    pub(super) params: RequestParams,
    pub(super) method: Method,
    /// The immutable body plan this route resolved to, in
    /// [`RequestBodyMode::Streaming`].
    ///
    /// Resolved here, by the stage holding the authority token, under the same
    /// host and child ceiling chain a buffered route resolves under: the route
    /// that chose the upstream is the route that chose the limit, and a
    /// streaming class that carried no plan would have to re-derive one from a
    /// selection already gone.
    ///
    /// Carried, not consumed. The bounded upload owner is its single admission
    /// consumer, and until that owner exists this path polls no frame under a
    /// limit and mints no permit owner.
    pub(super) plan: ResolvedBodyPlan,
}

/// The bounded multipart session one matched route will run.
///
/// One payload rather than three loose parameters, for the reason
/// [`StreamingProxyTarget`] is one: the route coordinator this feeds carries the
/// registration as a single value.
///
/// No method is carried. This class answers the request itself rather than
/// forwarding it, and the metadata-only request its gate and handler are given
/// is built from the same head classification read.
pub(super) struct StreamingMultipartTarget {
    /// The selected registration — the handler, and the bounds it reads its
    /// payload under, validated when the application built them.
    ///
    /// Shared, so classification costs a reference count bump rather than a
    /// copy of the bounds into every matched request's route class.
    pub(super) registration: Arc<MultipartRegistration>,
    /// The captured segments, bound here rather than at the gate: a multipart
    /// handler reads its own route parameters, so the one consumer that always
    /// wants them is this class's own.
    pub(super) params: RequestParams,
    /// The immutable body plan this route resolved to, in
    /// [`RequestBodyMode::Streaming`].
    ///
    /// Resolved by the stage holding the authority token, under the same host
    /// and child ceiling chain every other body-consuming route resolves under.
    pub(super) plan: ResolvedBodyPlan,
}

/// What a request head established before its body was read.
///
/// Held apart from [`RouteClass`] because the two answer different questions:
/// the class says how the wire must be read, and this says what a refusal found
/// while reading it can name.
struct Established {
    route: Arc<str>,
    protocol: RejectionProtocol,
}

/// The policy and identity a refusal before dispatch is answered under.
///
/// Body collection runs before any owned request exists, so a failure there has
/// no dispatch result to take a scope from. The selected mapper and the route
/// identity method selection established are carried out of classification
/// instead, so that failure is mapped by the same policy the handler's would
/// have been.
pub(super) struct PreBodyScope {
    mapper: Option<Arc<RejectionMapper>>,
    established: Option<Established>,
}

impl PreBodyScope {
    /// The policy a stage with no selected route answers under.
    fn unrouted(mapper: Option<Arc<RejectionMapper>>) -> Self {
        Self {
            mapper,
            established: None,
        }
    }

    /// Name the policy and identity one refusal before dispatch is mapped with.
    ///
    /// The identity is completed before the scope is built, never after. A
    /// scope holds its identity behind a shared handle, so establishing the
    /// route on a built one minted a second handle and dropped the first — an
    /// allocation and a free on every proxied stream, every head refusal, and
    /// every body-read refusal.
    pub(super) fn scope(&self, identity: RequestIdentity) -> RejectionScope {
        let identity = match &self.established {
            Some(established) => identity
                .with_route(Arc::clone(&established.route))
                .with_protocol(established.protocol),
            None => identity,
        };
        RejectionScope::new(self.mapper.clone(), identity)
    }
}

/// How one request head is read, and what a refusal while reading it can name.
pub(super) struct Classified<'a> {
    pub(super) class: RouteClass,
    pub(super) scope: PreBodyScope,
    /// The child router this head resolved to.
    ///
    /// Carried out rather than resolved a second time by the dispatch or the
    /// gate that follows: the authority parse and the binary search that
    /// selected it are exactly the two every class then repeated — the
    /// buffered and head-only dispatches, and the streaming proxy's middleware
    /// gate — on every request a `HostRouter` serves. `None` is a head no child
    /// claimed, which dispatch answers as a host terminal. An authority Camber
    /// could not parse never leaves here as `None` — it leaves as
    /// [`RouteClass::Refused`], answered from the head.
    pub(super) router: Option<&'a FrozenRouter>,
}

/// What a request head asks the connection to become.
///
/// Read where the hyper head is still in hand and carried into classification.
/// A streaming-proxy route that carries a WebSocket upgrade is dispatched as a
/// handshake, so it cannot pick its own class without knowing this, and a
/// forwarding target built before the question was asked was built to be
/// dropped.
#[derive(Clone, Copy)]
pub(super) enum HeadUpgrade {
    /// The head asks for no protocol change.
    None,
    /// The head carries a WebSocket upgrade request.
    WebSocket,
}

impl HeadUpgrade {
    /// Name what a head's upgrade headers asked for.
    pub(super) fn of(is_websocket: bool) -> Self {
        match is_websocket {
            true => Self::WebSocket,
            false => Self::None,
        }
    }

    fn is_websocket(self) -> bool {
        matches!(self, Self::WebSocket)
    }
}

/// Proof that this module's own route selection has already run.
///
/// Unforgeable outside this file: the only field is a private unit type, so no
/// sibling module can name it and none can mint one. A body plan can only be
/// built from a token, which is what keeps route identity, host resolution, and
/// handler selection the sole authorities for what a request's body policy is.
pub(super) struct ResolvedRouteAuthority(PrivateRouteSeal);

/// The private seal only this module can name.
struct PrivateRouteSeal;

/// Pre-body route classification result.
pub(super) enum RouteClass {
    /// Normal route — collect body into Request before dispatch, under the
    /// method the head was classified with and the body plan its route
    /// resolved to.
    Buffered {
        method: Method,
        plan: ResolvedBodyPlan,
    },
    /// Streaming proxy — forward hyper body directly to upstream.
    StreamingProxy(StreamingProxyTarget),
    /// Streaming multipart — the route coordinator owns a bounded session over
    /// the incoming body and hands its handler one access handle.
    StreamingMultipart(StreamingMultipartTarget),
    /// Head-only route (WebSocket, SSE) — dispatch from request metadata, skip body collection.
    HeadOnly,
    /// The routing stage already decided the answer.
    ///
    /// No body is read and no application handler runs, but a selected child
    /// router's middleware still surrounds the terminal, so dispatch is still
    /// where the answer is produced.
    Terminal,
    /// The request head already determines the refusal, so its body is not read.
    Refused(Rejected),
}

#[cfg(feature = "grpc")]
pub use super::grpc_support::GrpcRouter;

/// The dispatch class a matched handler establishes.
///
/// Read at method selection and nowhere else: this is the transition that makes
/// a protocol part of a request's identity, so a failure before it has none to
/// report and a failure after it reports the one that was selected.
fn protocol_of(handler: &RouteHandler) -> RejectionProtocol {
    match handler {
        // Ordinary HTTP, not a streaming class: the payload streams in, and the
        // answer is one buffered response the route's own mapper can shape.
        RouteHandler::Async(_) | RouteHandler::Multipart(_) => RejectionProtocol::OrdinaryHttp,
        RouteHandler::Stream(_) => RejectionProtocol::StreamingHttp,
        RouteHandler::Sse(_) => RejectionProtocol::ServerSentEvents,
        #[cfg(feature = "ws")]
        RouteHandler::WebSocket(_) => RejectionProtocol::WebSocket,
        RouteHandler::Proxy { .. } | RouteHandler::ProxyStream { .. } => RejectionProtocol::Proxy,
    }
}

/// Whether a matched route's upstream is currently failing its health check.
///
/// `None` is a route with no health check at all, which is never unhealthy.
/// One definition for the pre-body classification and the post-body dispatch:
/// two copies of the presence test, the negation, and the memory ordering are
/// what lets one path read a stale flag after the other is changed.
fn upstream_unhealthy(healthy: &Option<Arc<AtomicBool>>) -> bool {
    healthy
        .as_ref()
        .is_some_and(|flag| !flag.load(Ordering::Relaxed))
}

/// Immutable trie-based router. Created from Router::freeze().
pub(super) struct FrozenRouter {
    pub(super) root: FrozenNode,
    pub(super) middleware: Box<[MiddlewareFn]>,
    pub(super) skip_middleware_for_internal: bool,
    /// The rejection policy this router was configured with, frozen with it.
    ///
    /// One immutable shared allocation per router, because every concurrent
    /// request that reaches this router calls the same closure.
    pub(super) mapper: Option<Arc<RejectionMapper>>,
    /// The request-body ceiling this router was configured with, and whether it
    /// was configured at all. The distinction is what lets a host ceiling
    /// contain a child that set none.
    pub(super) body_ceiling: ConfiguredCeiling,
    /// The body-admission policy this router was configured with, frozen with
    /// it. One immutable shared allocation, read by every concurrent request
    /// this router answers.
    pub(super) body_policy: Option<Arc<BodyPolicy>>,
    #[cfg(feature = "grpc")]
    pub(super) grpc_router: Option<GrpcRouter>,
}

/// Result of routing a request through the frozen router.
pub(super) enum DispatchResult {
    Async(ResponseFuture, Request),
    Stream(
        Pin<Box<dyn Future<Output = StreamResponse> + Send>>,
        Request,
    ),
    Sse(SseHandler, Request),
    #[cfg(feature = "ws")]
    WebSocket(WsHandler, Request),
    #[cfg(feature = "ws")]
    ProxyWebSocket(Request, Arc<str>, Arc<str>),
    /// Streaming proxy: middleware gates the request, body streams with backpressure.
    ProxyStream(Request, Arc<str>, Arc<str>),
}

/// A dispatch that can only end in a buffered handler future.
///
/// Named apart from [`DispatchResult`] so the paths that produce nothing else —
/// the handler chain and the terminal fallbacks — say so in their return type.
/// A caller given the wider enum had to answer for streaming variants it can
/// never receive, and the only answer available was a status no counter moved
/// for.
pub(super) struct AsyncDispatch {
    pub(super) fut: ResponseFuture,
    pub(super) req: Request,
}

impl From<AsyncDispatch> for DispatchResult {
    fn from(dispatch: AsyncDispatch) -> Self {
        Self::Async(dispatch.fut, dispatch.req)
    }
}

impl DispatchResult {
    /// Whether this dispatch type needs a middleware gate check.
    /// Async already runs middleware inside dispatch.
    pub(super) fn needs_middleware_gate(&self) -> bool {
        match self {
            Self::Stream(..) | Self::Sse(..) | Self::ProxyStream(..) => true,
            #[cfg(feature = "ws")]
            Self::WebSocket(..) | Self::ProxyWebSocket(..) => true,
            Self::Async(..) => false,
        }
    }

    /// Whether this dispatch is a WebSocket upgrade (direct or proxied).
    #[cfg(feature = "ws")]
    pub(super) fn is_websocket(&self) -> bool {
        matches!(self, Self::WebSocket(..) | Self::ProxyWebSocket(..))
    }

    /// Borrow the request from any variant.
    pub(super) fn request_ref(&self) -> &Request {
        match self {
            Self::Async(_, req)
            | Self::Stream(_, req)
            | Self::Sse(_, req)
            | Self::ProxyStream(req, _, _) => req,
            #[cfg(feature = "ws")]
            Self::WebSocket(_, req) | Self::ProxyWebSocket(req, _, _) => req,
        }
    }
}

/// A middleware gate check for non-standard dispatch types, still to be run.
///
/// The gate runs middleware to determine whether the request should proceed.
/// A `Send` future that borrows from neither the router nor the request, so a
/// caller can hold it across an await.
pub(super) type GateCheck = ResponseFuture;

impl FrozenRouter {
    /// Classify a route before body collection.
    ///
    /// Returns `StreamingProxy` for proxy_stream routes so the incoming body
    /// can be forwarded without buffering. Routes with handlers return
    /// `Buffered`; unmatched and already-refused heads never read a body.
    ///
    /// A matched route also reports the identity it established, so a failure
    /// while reading the body names the route the handler would have run.
    ///
    /// Asks the trie to select and nothing more. This stage keeps only the
    /// matched case, so explaining a miss — the any-method pass and the `Allow`
    /// value it renders — would be built for every unrouted request and thrown
    /// away, then built again by [`Self::dispatch`].
    fn classify_route(
        &self,
        head: &RequestHead<'_>,
        upgrade: HeadUpgrade,
        outer: Option<ConfiguredCeiling>,
    ) -> (RouteClass, Option<Established>) {
        let path = head.path();
        let selected = match (head.routable_method(), split_path_segments(path)) {
            (Some(method), Some(segments)) => self
                .root
                .select(method, path, &segments)
                .map(|selected| (method, selected)),
            _ => None,
        };
        match selected {
            Some((method, selected)) => {
                let established = Established {
                    route: Arc::clone(selected.route),
                    protocol: protocol_of(selected.handler),
                };
                (
                    self.classify_matched(selected, method, upgrade, outer),
                    Some(established),
                )
            }
            None => (RouteClass::Terminal, None),
        }
    }

    /// How the wire must be read for the handler method selection chose.
    ///
    /// Takes the whole selection rather than its parameters, because only the
    /// streaming-proxy class keeps them: every other class discards the match,
    /// and binding a name to each captured segment for it boxed a string per
    /// parameter to throw away.
    ///
    /// The upgrade is answered here rather than after classification. A
    /// streaming-proxy route that carries one is dispatched as a handshake, so
    /// a forwarding target built for it was two `Arc` clones and a parameter
    /// binding produced to be dropped — and the trie was then walked a second
    /// time to re-derive the backend and prefix that target already held.
    fn classify_matched(
        &self,
        selected: Selected<'_, '_>,
        method: Method,
        upgrade: HeadUpgrade,
        outer: Option<ConfiguredCeiling>,
    ) -> RouteClass {
        let handler = selected.handler;
        let route = selected.route;
        match handler {
            RouteHandler::Proxy { healthy, .. } | RouteHandler::ProxyStream { healthy, .. }
                if upstream_unhealthy(healthy) =>
            {
                RouteClass::Refused(Rejected::no_admissible_backend())
            }
            // A proxied route asked to leave HTTP reads no body and forwards no
            // stream: the dispatch that follows answers it as the handshake its
            // registration cannot state. Both proxy kinds, because the answer
            // is the head's and not the registration's — read for the streaming
            // kind alone, the buffered kind spent a full body collection, and
            // up to `max_request_body` of heap, on a request classification had
            // already decided was a handshake.
            RouteHandler::Proxy { .. } | RouteHandler::ProxyStream { .. }
                if upgrade.is_websocket() =>
            {
                RouteClass::HeadOnly
            }
            RouteHandler::ProxyStream {
                backend, prefix, ..
            } => RouteClass::StreamingProxy(StreamingProxyTarget {
                backend: Arc::clone(backend),
                prefix: Arc::clone(prefix),
                params: self.gate_params(selected),
                method,
                plan: self.body_plan(route, RequestBodyMode::Streaming, outer),
            }),
            RouteHandler::Multipart(registration) => {
                RouteClass::StreamingMultipart(StreamingMultipartTarget {
                    registration: Arc::clone(registration),
                    params: selected.bind_params(),
                    plan: self.body_plan(route, RequestBodyMode::Streaming, outer),
                })
            }
            RouteHandler::Sse(_) => RouteClass::HeadOnly,
            #[cfg(feature = "ws")]
            RouteHandler::WebSocket(_) => RouteClass::HeadOnly,
            RouteHandler::Async(_) | RouteHandler::Stream(_) | RouteHandler::Proxy { .. } => {
                RouteClass::Buffered {
                    method,
                    plan: self.body_plan(route, RequestBodyMode::Buffered, outer),
                }
            }
        }
    }

    /// The body plan a matched body-consuming route resolves to.
    ///
    /// Minted here, immediately after selection, because holding the authority
    /// token is what a plan is built from. `outer` is the ceiling containing
    /// this router — a host router's own, or nothing for a single router — and
    /// this router's configured ceiling can only narrow it.
    fn body_plan(
        &self,
        route: &Arc<str>,
        mode: RequestBodyMode,
        outer: Option<ConfiguredCeiling>,
    ) -> ResolvedBodyPlan {
        ResolvedBodyPlan::new(
            &self.resolved_route(),
            Arc::clone(route),
            mode,
            self.body_ceiling,
            outer,
            self.body_policy.clone(),
        )
    }

    /// Attest that this router's own selection has already run.
    ///
    /// The one place a token is minted, on a `&self` a selection produced, so
    /// nothing that has not routed a request can build one.
    fn resolved_route(&self) -> ResolvedRouteAuthority {
        ResolvedRouteAuthority(PrivateRouteSeal)
    }

    /// The captured parameters this route's middleware gate will be given.
    ///
    /// Empty when this router registered no middleware. The gate is their only
    /// reader — [`Self::middleware_gate_head`] drops them on the un-middlewared
    /// arm, and `IncomingProxyParts` carries no parameter field for the
    /// forwarder to read — and a proxy route always registers a wildcard
    /// capture, so binding them there boxed a string per request for a value
    /// nothing looks at.
    fn gate_params(&self, selected: Selected<'_, '_>) -> RequestParams {
        match self.middleware.is_empty() {
            true => RequestParams::default(),
            false => selected.bind_params(),
        }
    }

    /// Route one built request, and name the policy its answer is mapped under.
    ///
    /// The lookup happens once, here, and its result establishes the route
    /// identity and dispatch class the scope carries. Nothing downstream
    /// repeats it, so a mapper and a handler cannot disagree about which route
    /// answered.
    pub(super) fn dispatch(
        &self,
        mut req: Request,
        mapper: Option<Arc<RejectionMapper>>,
    ) -> (DispatchResult, RejectionScope) {
        let identity = RequestIdentity::from_request(&req);
        // Copied to a local so the borrow of `req` is released before the arms
        // that move it.
        let path: Box<str> = req.path().into();
        let lookup = match split_path_segments(&path) {
            Some(segments) => self.root.lookup(req.method_enum(), &path, &segments),
            None => {
                let refusal = Rejected::uri_too_deep(PATH_SEGMENT_LIMIT);
                return self.terminal(req, mapper, identity, refusal);
            }
        };

        match lookup {
            RouteLookup::Matched(selected) => {
                let route = Arc::clone(selected.route);
                let handler = selected.handler;
                req.set_params(selected.bind_params());
                let scope = RejectionScope::new(
                    mapper,
                    identity
                        .with_route(route)
                        .with_protocol(protocol_of(handler)),
                );
                (self.dispatch_matched(handler, req, &scope), scope)
            }
            RouteLookup::MethodMismatch { route, allow } => {
                let refusal = Rejected::method_not_allowed(req.method(), &allow);
                self.terminal(req, mapper, identity.with_route(route), refusal)
            }
            RouteLookup::Unmatched => {
                let refusal = Rejected::no_route();
                self.terminal(req, mapper, identity, refusal)
            }
        }
    }

    /// Dispatch a request to the handler method selection chose.
    fn dispatch_matched(
        &self,
        handler: &RouteHandler,
        req: Request,
        scope: &RejectionScope,
    ) -> DispatchResult {
        match handler {
            RouteHandler::Async(handler) => self.dispatch_async(handler, req, scope.clone()).into(),
            RouteHandler::Stream(handler) => {
                let fut = handler(&req);
                DispatchResult::Stream(fut, req)
            }
            RouteHandler::Multipart(_) => Self::misdispatched(req, scope),
            RouteHandler::Sse(handler) => DispatchResult::Sse(Arc::clone(handler), req),
            #[cfg(feature = "ws")]
            RouteHandler::WebSocket(handler) => DispatchResult::WebSocket(Arc::clone(handler), req),
            RouteHandler::Proxy {
                backend,
                prefix,
                healthy,
            } => {
                self.dispatch_proxy_route(ProxyKind::Buffered, req, backend, prefix, healthy, scope)
            }
            RouteHandler::ProxyStream {
                backend,
                prefix,
                healthy,
            } => self.dispatch_proxy_route(
                ProxyKind::Streaming,
                req,
                backend,
                prefix,
                healthy,
                scope,
            ),
        }
    }

    /// Answer a streaming multipart route that reached buffered dispatch.
    ///
    /// Unreachable by construction: classification selects this class from the
    /// head, and the wire path answers it before an owned request exists. Named
    /// rather than folded into one of the handler arms, because a multipart
    /// route that fell through to buffered dispatch would run its handler with
    /// no session at all — and a `_` arm here would do exactly that silently.
    fn misdispatched(req: Request, scope: &RejectionScope) -> DispatchResult {
        let refusal = scope.clone();
        DispatchResult::Async(
            Box::pin(async move { refusal.map(Rejected::multipart_misdispatched()) }),
            req,
        )
    }

    /// Dispatch a proxied route, or refuse it while its upstream is unhealthy.
    fn dispatch_proxy_route(
        &self,
        kind: ProxyKind,
        req: Request,
        backend: &Arc<str>,
        prefix: &Arc<str>,
        healthy: &Option<Arc<AtomicBool>>,
        scope: &RejectionScope,
    ) -> DispatchResult {
        match upstream_unhealthy(healthy) {
            true => {
                let refusal = scope.clone();
                DispatchResult::Async(
                    Box::pin(async move { refusal.map(Rejected::no_admissible_backend()) }),
                    req,
                )
            }
            false => self.dispatch_proxy(kind, req, backend, prefix, scope),
        }
    }

    /// Answer a routing terminal inside this router's own middleware chain.
    ///
    /// The refusal is mapped at the terminal, so the mapped response unwinds
    /// through the frames this router entered around it — the same path a
    /// handler's own failure takes.
    fn terminal(
        &self,
        req: Request,
        mapper: Option<Arc<RejectionMapper>>,
        identity: RequestIdentity,
        rejected: Rejected,
    ) -> (DispatchResult, RejectionScope) {
        let scope = RejectionScope::new(mapper, identity);
        let next = Next::new(
            &self.middleware,
            Terminal::Rejected(rejected),
            scope.clone(),
        );
        let fut = next.call(&req);
        (DispatchResult::Async(fut, req), scope)
    }

    /// Dispatch a proxied route of either kind.
    ///
    /// The upgrade pre-check is asked once here rather than once per proxy
    /// kind: a request that asks to leave HTTP is the same answer whichever
    /// kind matched, and both would otherwise restate it along with the pair of
    /// `Arc` clones it takes.
    ///
    /// It is asked here at all because this stage names the dispatch variant
    /// and holds no head to name it from. Classification decides whether a body
    /// is read; this decides which variant answers, and the hyper head it would
    /// need is gone by the time an owned `Request` exists. The two predicates
    /// cannot disagree — both resolve the `Upgrade` header through one rule in
    /// `ws_proxy` — so the class that skipped the body and the variant selected
    /// here always name the same request.
    fn dispatch_proxy(
        &self,
        kind: ProxyKind,
        req: Request,
        backend: &Arc<str>,
        prefix: &Arc<str>,
        scope: &RejectionScope,
    ) -> DispatchResult {
        #[cfg(feature = "ws")]
        if super::ws_proxy::is_ws_upgrade_request(&req) {
            return DispatchResult::ProxyWebSocket(req, Arc::clone(backend), Arc::clone(prefix));
        }

        match kind {
            ProxyKind::Buffered => {
                dispatch_proxy_through_middleware(self, req, backend, prefix, scope).into()
            }
            // The gate mechanism, not a wrapped response: middleware gates the
            // request and the body streams with backpressure.
            ProxyKind::Streaming => {
                DispatchResult::ProxyStream(req, Arc::clone(backend), Arc::clone(prefix))
            }
        }
    }

    pub(super) fn dispatch_async(
        &self,
        handler: &Handler,
        req: Request,
        scope: RejectionScope,
    ) -> AsyncDispatch {
        let next = Next::new(&self.middleware, Terminal::Handler(handler), scope);
        let fut = next.call(&req);
        AsyncDispatch { fut, req }
    }

    /// Build a middleware gate check for non-standard dispatch types (WS, SSE, Stream).
    ///
    /// Returns `None` if no middleware is registered. Otherwise returns a `GateCheck`
    /// containing a `Send` future. The returned value does not borrow from the router
    /// or request, avoiding `Send` issues.
    pub(super) fn middleware_gate(
        &self,
        req: &Request,
        scope: &RejectionScope,
    ) -> Option<GateCheck> {
        match self.middleware.is_empty() {
            true => None,
            false => Some(self.gate_chain(req, scope)),
        }
    }

    /// Build a middleware gate check from borrowed request-head metadata.
    ///
    /// Defers Request construction until after confirming middleware exists,
    /// avoiding URI and HeaderMap clones when no middleware is registered.
    pub(super) fn middleware_gate_head(
        &self,
        head: &RequestHead<'_>,
        params: Option<RequestParams>,
        scope: &RejectionScope,
    ) -> Option<GateCheck> {
        match self.middleware.is_empty() {
            true => None,
            false => {
                // Built only on this arm: the URI and HeaderMap clones are
                // wasted work when no middleware is registered to read them.
                let gate_req = head.to_request(params);
                Some(self.gate_chain(&gate_req, scope))
            }
        }
    }

    /// This router's chain over one request, ending in the gate terminal.
    ///
    /// One place decides what a gate terminal is, so the two entry points above
    /// cannot drift apart in what they run. Called only from their populated
    /// arms, so the emptiness test each already made is not repeated here.
    fn gate_chain(&self, req: &Request, scope: &RejectionScope) -> GateCheck {
        let next = Next::new(&self.middleware, Terminal::Gate, scope.clone());
        next.call(req)
    }
}

/// Which proxy kind a matched route dispatches as, once the upgrade
/// pre-check both kinds share has answered.
enum ProxyKind {
    /// `Terminal::Proxy` through the middleware chain, response buffered.
    Buffered,
    /// Gated by middleware, body streamed with backpressure.
    Streaming,
}

/// Dispatch a proxy request through the middleware chain.
///
/// Uses `Terminal::Proxy` so the middleware chain forwards directly without
/// boxing a closure per request.
fn dispatch_proxy_through_middleware(
    router: &FrozenRouter,
    req: Request,
    backend: &Arc<str>,
    prefix: &Arc<str>,
    scope: &RejectionScope,
) -> AsyncDispatch {
    let terminal = Terminal::Proxy {
        backend: Arc::clone(backend),
        prefix: Arc::clone(prefix),
    };
    let next = Next::new(&router.middleware, terminal, scope.clone());
    let fut = next.call(&req);
    AsyncDispatch { fut, req }
}

/// Convert a gate check result into `Option<Response>`.
///
/// `None` means the chain passed the request through; `Some` is the answer it
/// gave instead. Read off the response's own provenance rather than a flag the
/// terminal sets: a frame that refuses after the terminal has already been
/// reached replaces the terminal's value, and a reached-flag cannot tell that
/// from a pass — it would forward a request its own gate had just refused.
pub(super) fn gate_result(resp: Response) -> Option<Response> {
    match resp.provenance().is_gate_passthrough() {
        true => None,
        false => Some(resp),
    }
}

/// A routed request, and the router that answered it.
///
/// The router is carried out of dispatch rather than resolved a second time:
/// a gate check on the same request would otherwise repeat the Host-header
/// parse and the binary search routing already did. `None` is a request no
/// router claimed — the fallback arms — which has no gate to run either.
pub(super) struct Routed<'a> {
    pub(super) result: DispatchResult,
    pub(super) router: Option<&'a FrozenRouter>,
    /// The rejection policy and identity this request resolved to, selected
    /// once here so no later stage repeats the host or route lookup.
    pub(super) scope: RejectionScope,
}

/// The child router one request resolves to, and the refusal an authority that
/// is not one earned.
///
/// Named so a stage that answers twice against one request — a scope and then a
/// dispatch — can resolve once and hand the same answer to both. The two
/// negative cases stay distinct: "no router claims this authority" is a
/// `Routing` `404`, and "this is not an authority" is a `Routing` `400`.
pub(super) type Resolution<'a> = Result<Option<&'a FrozenRouter>, Rejected>;

/// Routes requests to the correct FrozenRouter.
pub(super) enum ServerDispatch {
    Single(FrozenRouter),
    Host(FrozenHostRouter),
}

impl ServerDispatch {
    /// Classify a route from request-head metadata before body collection.
    ///
    /// A Host header that resolves to no router at all is unmatched. A Host
    /// header that is itself invalid carries the refusal it already earned.
    ///
    /// The policy the resolved router selects leaves with the class, so a
    /// refusal raised while reading the body is answered by the same mapper the
    /// route's own failure would have reached.
    pub(super) fn classify_route<'a>(
        &'a self,
        head: &RequestHead<'_>,
        upgrade: HeadUpgrade,
    ) -> Classified<'a> {
        match self.resolve_from_head(head) {
            Ok(Some(router)) => {
                let (class, established) =
                    router.classify_route(head, upgrade, self.outer_ceiling());
                Classified {
                    class,
                    scope: PreBodyScope {
                        mapper: self.select_mapper(Some(router)),
                        established,
                    },
                    router: Some(router),
                }
            }
            Ok(None) => Classified {
                class: RouteClass::Terminal,
                scope: PreBodyScope::unrouted(self.select_mapper(None)),
                router: None,
            },
            // Carried, not folded into the terminal above. An authority Camber
            // could not parse is answered from the head: reading it as "no
            // router" spent an authority parse, a header-map clone, a URI clone
            // and a second construction of this same refusal to arrive at the
            // answer already in hand.
            Err(rejected) => Classified {
                class: RouteClass::Refused(rejected),
                scope: PreBodyScope::unrouted(self.select_mapper(None)),
                router: None,
            },
        }
    }

    /// The request-body ceiling that contains every child router here.
    ///
    /// `None` for a single router: nothing contains it, so its own configured
    /// ceiling is the whole answer. A host router's ceiling applies to every
    /// child, and a child that configured one can only narrow it.
    fn outer_ceiling(&self) -> Option<ConfiguredCeiling> {
        match self {
            Self::Single(_) => None,
            Self::Host(hosts) => Some(hosts.body_ceiling()),
        }
    }

    /// Find the router a borrowed head names, or the refusal it earned.
    ///
    /// The refusal is threaded rather than collapsed into "no router": the two
    /// answers are not the same question, and every caller here treats them
    /// differently.
    pub(super) fn resolve_from_head(&self, head: &RequestHead<'_>) -> Resolution<'_> {
        match self {
            Self::Single(router) => Ok(Some(router)),
            Self::Host(host_router) => host_router.resolve_from_head(head),
        }
    }

    /// Find the router a built request names, or the refusal it earned.
    pub(super) fn resolve(&self, req: &Request) -> Resolution<'_> {
        match self {
            Self::Single(router) => Ok(Some(router)),
            Self::Host(host_router) => host_router.resolve(req),
        }
    }

    /// The child router an authority selects, for a stage that answers without
    /// one.
    ///
    /// Scope selection asks only which mapper answers. [`Self::resolve`] mints
    /// a refusal to tell an authority Camber cannot parse from one no child
    /// claims, and a caller that needs neither dropped it unread — a `format!`
    /// detail and a shared allocation per malformed request, for a value
    /// nothing records.
    fn router_for(&self, authority: &str) -> Option<&FrozenRouter> {
        match self {
            Self::Single(router) => Some(router),
            Self::Host(host_router) => host_router.router_for(authority),
        }
    }

    /// The rejection policy a resolved router selects.
    ///
    /// A resolved child router's own mapper wins; a host router's mapper
    /// answers for every child that configured none, and for a Host that
    /// selected no child at all; the built-in mapper answers when neither
    /// exists.
    fn select_mapper(&self, router: Option<&FrozenRouter>) -> Option<Arc<RejectionMapper>> {
        let child = router.and_then(|router| router.mapper.clone());
        match self {
            Self::Single(_) => child,
            Self::Host(hosts) => child.or_else(|| hosts.mapper()),
        }
    }

    /// The policy a stage with no resolved child router answers under.
    pub(super) fn host_scope(&self, identity: RequestIdentity) -> RejectionScope {
        RejectionScope::new(self.select_mapper(None), identity)
    }

    /// The policy a stage that has only a borrowed head answers under.
    ///
    /// An authority that resolves no child falls back to the host or built-in
    /// mapper, which is the same precedence a dispatched request follows.
    pub(super) fn head_scope(
        &self,
        head: &RequestHead<'_>,
        identity: RequestIdentity,
    ) -> RejectionScope {
        RejectionScope::new(
            self.select_mapper(self.router_for(head.authority())),
            identity,
        )
    }

    /// The policy a stage that has already resolved its child answers under.
    ///
    /// Takes the resolution rather than repeating it, so a stage that answers
    /// twice against one request — a scope and then a dispatch — parses the
    /// authority once. An authority Camber cannot parse selects no child, which
    /// is the same mapper an authority no child claims falls back to; telling
    /// those two apart is the caller's question, not this one's.
    pub(super) fn resolved_head_scope(
        &self,
        resolved: &Resolution<'_>,
        identity: RequestIdentity,
    ) -> RejectionScope {
        RejectionScope::new(
            self.select_mapper(resolved.as_ref().ok().copied().flatten()),
            identity,
        )
    }

    /// The same policy, for a stage that has already built its request.
    pub(super) fn resolved_scope(
        &self,
        resolved: &Resolution<'_>,
        req: &Request,
    ) -> RejectionScope {
        self.resolved_head_scope(resolved, RequestIdentity::from_request(req))
    }

    /// Route one built request through the child its head already resolved.
    ///
    /// Named apart from a resolving entry point so classification's authority
    /// parse and binary search are not repeated here, on every buffered and
    /// head-only request a `HostRouter` serves. `None` is a head no child
    /// claimed; an authority Camber could not parse never reaches here, because
    /// classification answers that one from the head.
    pub(super) fn dispatch_resolved<'a>(
        &'a self,
        req: Request,
        router: Option<&'a FrozenRouter>,
    ) -> Routed<'a> {
        match router {
            Some(router) => {
                let (result, scope) = router.dispatch(req, self.select_mapper(Some(router)));
                Routed {
                    result,
                    router: Some(router),
                    scope,
                }
            }
            None => self.host_terminal(req, Rejected::not_found("no router claims this authority")),
        }
    }

    /// Answer a refusal found before any child router was selected.
    ///
    /// There is no child chain to unwind through, so none runs: middleware a
    /// request never entered cannot wrap its refusal.
    fn host_terminal(&self, req: Request, rejected: Rejected) -> Routed<'_> {
        let scope = self.host_scope(RequestIdentity::from_request(&req));
        let mapping = scope.clone();
        let fut: ResponseFuture = Box::pin(async move { mapping.map(rejected) });
        Routed {
            result: DispatchResult::Async(fut, req),
            router: None,
            scope,
        }
    }

    /// Dispatch a request through the middleware chain with a given handler.
    ///
    /// Every arm below is a buffered handler future, so the caller is told that
    /// in the return type rather than being handed the wider enum and left to
    /// invent an answer for variants it cannot receive.
    ///
    /// Takes the resolution its caller's scope was already selected from, so
    /// `/health` and `/metrics` — the highest-frequency paths a served process
    /// sees — parse their authority and search the host table once rather than
    /// once per answer.
    pub(super) fn dispatch_with_handler(
        resolved: Resolution<'_>,
        handler: &Handler,
        req: Request,
        scope: RejectionScope,
    ) -> AsyncDispatch {
        match resolved {
            Ok(Some(router)) => router.dispatch_async(handler, req, scope),
            Ok(None) => Self::refuse(req, scope, Rejected::no_route()),
            Err(rejected) => Self::refuse(req, scope, rejected),
        }
    }

    /// Answer a refusal with no middleware chain to unwind through.
    fn refuse(req: Request, scope: RejectionScope, rejected: Rejected) -> AsyncDispatch {
        let fut: ResponseFuture = Box::pin(async move { scope.map(rejected) });
        AsyncDispatch { fut, req }
    }

    /// Whether internal routes should bypass middleware.
    pub(super) fn skip_middleware_for_internal(&self) -> bool {
        match self {
            Self::Single(router) => router.skip_middleware_for_internal,
            Self::Host(_) => false,
        }
    }

    #[cfg(feature = "grpc")]
    pub(super) fn grpc_router(&self) -> Option<&super::grpc_support::GrpcRouter> {
        match self {
            Self::Single(router) => router.grpc_router.as_ref(),
            Self::Host(_) => None,
        }
    }
}
