use super::host_router::FrozenHostRouter;
use super::middleware::{MiddlewareFn, Next, Terminal};
use super::request::{Params as RequestParams, RequestHead};
use super::stream::StreamResponse;
pub(super) use super::trie::Handler;
pub(super) use super::trie::SseHandler;
#[cfg(feature = "ws")]
pub(super) use super::trie::WsHandler;
use super::trie::{FrozenNode, RouteHandler, split_path_segments};
use super::{Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

/// The upstream a streaming-proxy route resolved to.
///
/// One payload rather than three loose parameters, so the dispatch call it
/// feeds carries the route as a single value.
pub(super) struct StreamingProxyTarget {
    pub(super) backend: Arc<str>,
    pub(super) prefix: Arc<str>,
    pub(super) params: RequestParams,
}

/// Pre-body route classification result.
pub(super) enum RouteClass {
    /// Normal route — collect body into Request before dispatch.
    Buffered,
    /// Streaming proxy — forward hyper body directly to upstream.
    StreamingProxy(StreamingProxyTarget),
    /// Head-only route (WebSocket, SSE) — dispatch from request metadata, skip body collection.
    HeadOnly,
    /// No route matches. Middleware still runs, but no body can reach a handler.
    Unmatched,
    /// The request head already determines the response, so its body is not read.
    Refused(Response),
}

#[cfg(feature = "grpc")]
pub use super::grpc_support::GrpcRouter;

/// Handler that returns 404 for unmatched routes, allowing middleware to still run.
static NOT_FOUND_HANDLER: LazyLock<Handler> = LazyLock::new(|| {
    Box::new(|_req: &Request| {
        Box::pin(async { not_found() }) as Pin<Box<dyn Future<Output = Response> + Send>>
    })
});

/// The answer to a path no route claims.
///
/// One constructor for the handler that runs middleware first and the fallback
/// that does not: the two differ only in whether the chain is reached, never in
/// what the peer is told.
fn not_found() -> Response {
    Response::text_raw(404, "not found")
}

/// The answer to a route whose upstream is failing its health check.
///
/// Shared with the pre-body path in `handle`, which refuses the same route
/// class before a body is read. Both refusals name the same condition, so both
/// come from here.
pub(super) fn service_unavailable() -> Response {
    Response::text_raw(503, "service unavailable")
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
    #[cfg(feature = "grpc")]
    pub(super) grpc_router: Option<GrpcRouter>,
}

/// Result of routing a request through the frozen router.
pub(super) enum DispatchResult {
    Async(Pin<Box<dyn Future<Output = Response> + Send>>, Request),
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
    pub(super) fut: Pin<Box<dyn Future<Output = Response> + Send>>,
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

/// Result of a middleware gate check for non-standard dispatch types.
///
/// The gate runs middleware to determine if the request should proceed.
/// Returns a `Send` future that does not borrow from the router or request.
pub(super) struct GateCheck {
    pub(super) reached: Arc<AtomicBool>,
    pub(super) fut: Pin<Box<dyn Future<Output = Response> + Send>>,
}

impl FrozenRouter {
    /// Classify a route before body collection.
    ///
    /// Returns `StreamingProxy` for proxy_stream routes so the incoming body
    /// can be forwarded without buffering. Routes with handlers return
    /// `Buffered`; unmatched and already-refused heads never read a body.
    pub(super) fn classify_route(&self, head: &RequestHead<'_>) -> RouteClass {
        let path = head.path();
        let segments = match split_path_segments(path) {
            Some(s) => s,
            None => return RouteClass::Refused(Response::text_raw(414, "URI too long")),
        };
        match self.root.lookup(head.method(), path, &segments) {
            Some((RouteHandler::Proxy { healthy, .. }, _))
            | Some((RouteHandler::ProxyStream { healthy, .. }, _))
                if upstream_unhealthy(healthy) =>
            {
                RouteClass::Refused(service_unavailable())
            }
            Some((
                RouteHandler::ProxyStream {
                    backend, prefix, ..
                },
                params,
            )) => RouteClass::StreamingProxy(StreamingProxyTarget {
                backend: Arc::clone(backend),
                prefix: Arc::clone(prefix),
                params,
            }),
            Some((RouteHandler::Sse(_), _)) => RouteClass::HeadOnly,
            #[cfg(feature = "ws")]
            Some((RouteHandler::WebSocket(_), _)) => RouteClass::HeadOnly,
            Some(_) => RouteClass::Buffered,
            None => RouteClass::Unmatched,
        }
    }

    pub(super) fn dispatch(&self, mut req: Request) -> DispatchResult {
        let method = req.method_enum();

        // Copy path to a local so the borrow of `req` is released before
        // the match arms that need to move `req`.
        let path_owned: Box<str> = req.path().into();
        let result = {
            let segments = match split_path_segments(&path_owned) {
                Some(s) => s,
                None => {
                    let fut = Box::pin(async { Response::text_raw(414, "URI too long") });
                    return DispatchResult::Async(fut, req);
                }
            };
            self.root.lookup(method, &path_owned, &segments)
        };

        match result {
            Some((RouteHandler::Async(handler), params)) => {
                req.set_params(params);
                self.dispatch_async(handler, req).into()
            }
            Some((RouteHandler::Stream(handler), params)) => {
                req.set_params(params);
                let fut = handler(&req);
                DispatchResult::Stream(fut, req)
            }
            Some((RouteHandler::Sse(handler), params)) => {
                req.set_params(params);
                DispatchResult::Sse(Arc::clone(handler), req)
            }
            #[cfg(feature = "ws")]
            Some((RouteHandler::WebSocket(handler), params)) => {
                req.set_params(params);
                DispatchResult::WebSocket(Arc::clone(handler), req)
            }
            Some((RouteHandler::Proxy { healthy, .. }, _))
            | Some((RouteHandler::ProxyStream { healthy, .. }, _))
                if upstream_unhealthy(healthy) =>
            {
                let fut = Box::pin(async { service_unavailable() });
                DispatchResult::Async(fut, req)
            }
            Some((
                RouteHandler::Proxy {
                    backend, prefix, ..
                },
                params,
            )) => {
                req.set_params(params);
                self.dispatch_proxy(ProxyKind::Buffered, req, backend, prefix)
            }
            Some((
                RouteHandler::ProxyStream {
                    backend, prefix, ..
                },
                params,
            )) => {
                req.set_params(params);
                self.dispatch_proxy(ProxyKind::Streaming, req, backend, prefix)
            }
            None => self.dispatch_async(&NOT_FOUND_HANDLER, req).into(),
        }
    }

    /// Dispatch a proxied route of either kind.
    ///
    /// The upgrade pre-check is asked once here rather than once per proxy
    /// kind: a request that asks to leave HTTP is the same answer whichever
    /// kind matched, and both would otherwise restate it along with the pair of
    /// `Arc` clones it takes.
    fn dispatch_proxy(
        &self,
        kind: ProxyKind,
        req: Request,
        backend: &Arc<str>,
        prefix: &Arc<str>,
    ) -> DispatchResult {
        #[cfg(feature = "ws")]
        if super::ws_proxy::is_ws_upgrade_request(&req) {
            return DispatchResult::ProxyWebSocket(req, Arc::clone(backend), Arc::clone(prefix));
        }

        match kind {
            ProxyKind::Buffered => {
                dispatch_proxy_through_middleware(self, req, backend, prefix).into()
            }
            // The gate mechanism, not a wrapped response: middleware gates the
            // request and the body streams with backpressure.
            ProxyKind::Streaming => {
                DispatchResult::ProxyStream(req, Arc::clone(backend), Arc::clone(prefix))
            }
        }
    }

    pub(super) fn dispatch_async(&self, handler: &Handler, req: Request) -> AsyncDispatch {
        let terminal = Terminal::Handler(handler);
        let next = Next::new(&self.middleware, terminal);
        let fut = next.call(&req);
        AsyncDispatch { fut, req }
    }

    /// Build a middleware gate check for non-standard dispatch types (WS, SSE, Stream).
    ///
    /// Returns `None` if no middleware is registered. Otherwise returns a `GateCheck`
    /// containing a `Send` future. The returned value does not borrow from the router
    /// or request, avoiding `Send` issues.
    pub(super) fn middleware_gate(&self, req: &Request) -> Option<GateCheck> {
        match self.middleware.is_empty() {
            true => None,
            false => {
                let reached = Arc::new(AtomicBool::new(false));
                let flag = Arc::clone(&reached);
                let terminal = Terminal::Gate(flag);
                let next = Next::new(&self.middleware, terminal);
                let fut = next.call(req);
                Some(GateCheck { reached, fut })
            }
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
    ) -> Option<GateCheck> {
        match self.middleware.is_empty() {
            true => None,
            false => {
                // Built only on this arm: the URI and HeaderMap clones are
                // wasted work when no middleware is registered to read them.
                let gate_req = head.to_request(params);
                self.middleware_gate(&gate_req)
            }
        }
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
) -> AsyncDispatch {
    let terminal = Terminal::Proxy {
        backend: Arc::clone(backend),
        prefix: Arc::clone(prefix),
    };
    let next = Next::new(&router.middleware, terminal);
    let fut = next.call(&req);
    AsyncDispatch { fut, req }
}

/// Convert a gate check result into `Option<Response>`.
/// `None` means middleware passed through; `Some` means it short-circuited.
pub(super) fn gate_result(reached: Arc<AtomicBool>, resp: Response) -> Option<Response> {
    match reached.load(Ordering::Acquire) {
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
}

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
    pub(super) fn classify_route(&self, head: &RequestHead<'_>) -> RouteClass {
        match self.resolve_from_head(head) {
            Ok(Some(router)) => router.classify_route(head),
            Ok(None) => RouteClass::Unmatched,
            Err(refusal) => RouteClass::Refused(refusal),
        }
    }

    /// Find the router a borrowed head names, or the refusal it earned.
    ///
    /// The refusal is threaded rather than collapsed into "no router": the two
    /// answers are not the same question, and every caller here treats them
    /// differently.
    fn resolve_from_head(&self, head: &RequestHead<'_>) -> Result<Option<&FrozenRouter>, Response> {
        match self {
            Self::Single(router) => Ok(Some(router)),
            Self::Host(host_router) => host_router.resolve_from_head(head),
        }
    }

    fn resolve(&self, req: &Request) -> Result<Option<&FrozenRouter>, Response> {
        match self {
            Self::Single(router) => Ok(Some(router)),
            Self::Host(host_router) => host_router.resolve(req),
        }
    }

    /// Build a terminal `DispatchResult` from an error response.
    /// Pass `None` for a 404; pass `Some(resp)` for a host-resolution error.
    fn fallback(error_resp: Option<Response>, req: Request) -> AsyncDispatch {
        let fut: Pin<Box<dyn Future<Output = Response> + Send>> = match error_resp {
            None => Box::pin(async { not_found() }),
            Some(resp) => Box::pin(async move { resp }),
        };
        AsyncDispatch { fut, req }
    }

    pub(super) fn dispatch(&self, req: Request) -> Routed<'_> {
        match self.resolve(&req) {
            Ok(Some(router)) => Routed {
                result: router.dispatch(req),
                router: Some(router),
            },
            Ok(None) => Routed {
                result: Self::fallback(None, req).into(),
                router: None,
            },
            Err(resp) => Routed {
                result: Self::fallback(Some(resp), req).into(),
                router: None,
            },
        }
    }

    /// Run middleware gate from borrowed request-head metadata.
    ///
    /// Uses `resolve_from_head` to find the router without cloning,
    /// then defers Request construction to the router's gate method.
    ///
    /// A Host header that names no router at all has no gate to run, which is
    /// `Ok(None)`. A Host header that is invalid leaves as `Err(refusal)`: this
    /// gate guards the gRPC and streaming-proxy paths, so a caller reading an
    /// unresolvable host as a pass would forward the request upstream with the
    /// chain never run.
    pub(super) fn middleware_gate_head(
        &self,
        head: &RequestHead<'_>,
        params: Option<RequestParams>,
    ) -> Result<Option<GateCheck>, Response> {
        match self.resolve_from_head(head)? {
            Some(router) => Ok(router.middleware_gate_head(head, params)),
            None => Ok(None),
        }
    }

    /// Dispatch a request through the middleware chain with a given handler.
    ///
    /// Every arm below is a buffered handler future, so the caller is told that
    /// in the return type rather than being handed the wider enum and left to
    /// invent an answer for variants it cannot receive.
    pub(super) fn dispatch_with_handler(&self, handler: &Handler, req: Request) -> AsyncDispatch {
        match self.resolve(&req) {
            Ok(Some(router)) => router.dispatch_async(handler, req),
            Ok(None) => Self::fallback(None, req),
            Err(resp) => Self::fallback(Some(resp), req),
        }
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
