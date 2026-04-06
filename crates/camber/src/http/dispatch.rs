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

/// Pre-body route classification result.
pub(super) enum RouteClass {
    /// Normal route — collect body into Request before dispatch.
    Buffered,
    /// Streaming proxy — forward hyper body directly to upstream.
    StreamingProxy {
        backend: Arc<str>,
        prefix: Arc<str>,
        params: RequestParams,
    },
    /// Streaming proxy health check failed — return 503 without body collection.
    StreamingProxyUnhealthy,
    /// Head-only route (WebSocket, SSE) — dispatch from request metadata, skip body collection.
    HeadOnly,
}

#[cfg(feature = "grpc")]
pub use super::grpc_support::GrpcRouter;

/// Handler that returns 404 for unmatched routes, allowing middleware to still run.
static NOT_FOUND_HANDLER: LazyLock<Handler> = LazyLock::new(|| {
    Box::new(|_req: &Request| {
        Box::pin(async { Response::text_raw(404, "not found") })
            as Pin<Box<dyn Future<Output = Response> + Send>>
    })
});

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
    /// can be forwarded without buffering. All other routes return `Buffered`.
    pub(super) fn classify_route(&self, head: &RequestHead<'_>) -> RouteClass {
        let path = head.path();
        let segments = match split_path_segments(path) {
            Some(s) => s,
            None => return RouteClass::Buffered,
        };
        match self.root.lookup(head.method(), path, &segments) {
            Some((
                RouteHandler::ProxyStream {
                    healthy,
                    backend,
                    prefix,
                },
                _,
            )) if healthy.as_ref().is_some_and(|f| !f.load(Ordering::Relaxed)) => {
                RouteClass::StreamingProxyUnhealthy
            }
            Some((
                RouteHandler::ProxyStream {
                    backend, prefix, ..
                },
                params,
            )) => RouteClass::StreamingProxy {
                backend: Arc::clone(backend),
                prefix: Arc::clone(prefix),
                params,
            },
            Some((RouteHandler::Sse(_), _)) => RouteClass::HeadOnly,
            #[cfg(feature = "ws")]
            Some((RouteHandler::WebSocket(_), _)) => RouteClass::HeadOnly,
            _ => RouteClass::Buffered,
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
                self.dispatch_async(handler, req)
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
                if healthy.as_ref().is_some_and(|f| !f.load(Ordering::Relaxed)) =>
            {
                let fut = Box::pin(async { Response::text_raw(503, "service unavailable") });
                DispatchResult::Async(fut, req)
            }
            Some((
                RouteHandler::Proxy {
                    backend, prefix, ..
                },
                params,
            )) => {
                req.set_params(params);
                dispatch_proxy_through_middleware(self, req, backend, prefix)
            }
            Some((
                RouteHandler::ProxyStream {
                    backend, prefix, ..
                },
                params,
            )) => {
                req.set_params(params);
                dispatch_proxy_stream(req, backend, prefix)
            }
            None => self.dispatch_async(&NOT_FOUND_HANDLER, req),
        }
    }

    pub(super) fn dispatch_async(&self, handler: &Handler, req: Request) -> DispatchResult {
        let terminal = Terminal::Handler(handler);
        let next = Next::new(&self.middleware, terminal);
        let fut = next.call(&req);
        DispatchResult::Async(fut, req)
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
                let gate_req = head.to_gate_request(params);
                self.middleware_gate(&gate_req)
            }
        }
    }
}

/// Check if the request is a WebSocket upgrade.
#[cfg(feature = "ws")]
fn is_ws_upgrade(req: &Request) -> bool {
    req.headers()
        .any(|(k, v)| k.eq_ignore_ascii_case("upgrade") && v.eq_ignore_ascii_case("websocket"))
}

/// Dispatch a proxy request through the middleware chain.
///
/// For WebSocket upgrades (ws feature): returns `DispatchResult::ProxyWebSocket`
/// which uses the gate mechanism. For HTTP proxy: uses `Terminal::Proxy` so the
/// middleware chain forwards directly without boxing a closure per request.
fn dispatch_proxy_through_middleware(
    router: &FrozenRouter,
    req: Request,
    backend: &Arc<str>,
    prefix: &Arc<str>,
) -> DispatchResult {
    #[cfg(feature = "ws")]
    if is_ws_upgrade(&req) {
        return DispatchResult::ProxyWebSocket(req, Arc::clone(backend), Arc::clone(prefix));
    }

    let terminal = Terminal::Proxy {
        backend: Arc::clone(backend),
        prefix: Arc::clone(prefix),
    };
    let next = Next::new(&router.middleware, terminal);
    let fut = next.call(&req);
    DispatchResult::Async(fut, req)
}

/// Dispatch a streaming proxy request via the gate mechanism.
///
/// For WebSocket upgrades (ws feature): returns `DispatchResult::ProxyWebSocket`
/// which uses the same gate mechanism. For HTTP: returns `DispatchResult::ProxyStream`
/// so middleware gates the request without wrapping the streamed response.
fn dispatch_proxy_stream(req: Request, backend: &Arc<str>, prefix: &Arc<str>) -> DispatchResult {
    #[cfg(feature = "ws")]
    if is_ws_upgrade(&req) {
        return DispatchResult::ProxyWebSocket(req, Arc::clone(backend), Arc::clone(prefix));
    }

    DispatchResult::ProxyStream(req, Arc::clone(backend), Arc::clone(prefix))
}

/// Convert a gate check result into `Option<Response>`.
/// `None` means middleware passed through; `Some` means it short-circuited.
pub(super) fn gate_result(reached: Arc<AtomicBool>, resp: Response) -> Option<Response> {
    match reached.load(Ordering::Acquire) {
        true => None,
        false => Some(resp),
    }
}

/// Routes requests to the correct FrozenRouter.
pub(super) enum ServerDispatch {
    Single(FrozenRouter),
    Host(FrozenHostRouter),
}

impl ServerDispatch {
    /// Classify a route from request-head metadata before body collection.
    pub(super) fn classify_route(&self, head: &RequestHead<'_>) -> RouteClass {
        let router = match self.resolve_from_head(head) {
            Some(r) => r,
            None => return RouteClass::Buffered,
        };
        router.classify_route(head)
    }

    fn resolve_from_head(&self, head: &RequestHead<'_>) -> Option<&FrozenRouter> {
        match self {
            Self::Single(router) => Some(router),
            Self::Host(host_router) => host_router.resolve_from_head(head).ok().flatten(),
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
    fn fallback(error_resp: Option<Response>, req: Request) -> DispatchResult {
        let fut: Pin<Box<dyn Future<Output = Response> + Send>> = match error_resp {
            None => Box::pin(async { Response::text_raw(404, "not found") }),
            Some(resp) => Box::pin(async move { resp }),
        };
        DispatchResult::Async(fut, req)
    }

    pub(super) fn dispatch(&self, req: Request) -> DispatchResult {
        match self.resolve(&req) {
            Ok(Some(router)) => router.dispatch(req),
            Ok(None) => Self::fallback(None, req),
            Err(resp) => Self::fallback(Some(resp), req),
        }
    }

    /// Run middleware as a gate check for non-standard dispatch types.
    /// Returns `None` if no middleware or gate not needed. Does not borrow
    /// the request or dispatch result in the returned value.
    pub(super) fn middleware_gate(&self, req: &Request) -> Option<GateCheck> {
        match self.resolve(req) {
            Ok(Some(router)) => router.middleware_gate(req),
            _ => None,
        }
    }

    /// Run middleware gate from borrowed request-head metadata.
    ///
    /// Uses `resolve_from_head` to find the router without cloning,
    /// then defers Request construction to the router's gate method.
    pub(super) fn middleware_gate_head(
        &self,
        head: &RequestHead<'_>,
        params: Option<RequestParams>,
    ) -> Option<GateCheck> {
        let router = self.resolve_from_head(head)?;
        router.middleware_gate_head(head, params)
    }

    /// Dispatch a request through the middleware chain with a given handler.
    pub(super) fn dispatch_with_handler(&self, handler: &Handler, req: Request) -> DispatchResult {
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
