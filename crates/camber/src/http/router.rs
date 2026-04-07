use super::method::Method;
use super::middleware::MiddlewareFn;
use super::response::IntoResponse;
use super::sse::SseWriter;
use super::stream::StreamResponse;
use super::trie::{RouteHandler, TrieNode};
#[cfg(feature = "ws")]
use super::websocket::WsConn;
use super::{Request, Response};
use crate::RuntimeError;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::BufferConfig;

// Re-export dispatch types so existing `super::router::*` imports keep working.
#[cfg(feature = "grpc")]
pub use super::dispatch::GrpcRouter;
#[cfg(feature = "ws")]
pub(super) use super::dispatch::WsHandler;
pub(super) use super::dispatch::{
    DispatchResult, FrozenRouter, GateCheck, Handler, ServerDispatch, SseHandler, gate_result,
};

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("middleware_count", &self.middleware.len())
            .field("buffers", &self.buffers)
            .field(
                "skip_middleware_for_internal",
                &self.skip_middleware_for_internal,
            )
            .finish()
    }
}

/// Maps HTTP method + path pairs to handler functions.
///
/// Routes are inserted into a trie during registration, then frozen
/// via `freeze()` before serving. Static segments take priority over
/// parameterized segments (`:name`) during matching.
pub struct Router {
    root: TrieNode,
    middleware: Vec<MiddlewareFn>,
    buffers: BufferConfig,
    skip_middleware_for_internal: bool,
    #[cfg(feature = "grpc")]
    grpc_router: Option<super::dispatch::GrpcRouter>,
}

impl Default for Router {
    fn default() -> Self {
        Self {
            root: TrieNode::new(),
            middleware: Vec::new(),
            buffers: BufferConfig::default(),
            skip_middleware_for_internal: false,
            #[cfg(feature = "grpc")]
            grpc_router: None,
        }
    }
}

impl Router {
    /// Create an empty router with default buffer settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum request body size in bytes (capped at 256 MB).
    #[must_use]
    pub fn max_request_body(mut self, bytes: usize) -> Self {
        self.buffers = self.buffers.with_max_request_body(bytes);
        self
    }

    /// Set the channel buffer size for SSE connections.
    ///
    /// Controls how many events can be queued before backpressure applies.
    /// Default: 32.
    #[must_use]
    pub fn sse_buffer_size(mut self, size: usize) -> Self {
        self.buffers = self.buffers.with_sse_buffer_size(size);
        self
    }

    /// Set the channel buffer size for WebSocket connections.
    ///
    /// Controls how many messages can be queued in each direction before
    /// backpressure applies. Default: 32.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn ws_buffer_size(mut self, size: usize) -> Self {
        self.buffers = self.buffers.with_ws_buffer_size(size);
        self
    }

    pub(super) fn buffer_config(&self) -> BufferConfig {
        self.buffers
    }

    /// Skip middleware for internal routes (`/health`, `/metrics`, `/debug/pprof/cpu`).
    ///
    /// Default: `false` (middleware applies to all routes including internal ones).
    /// Set to `true` to restore the pre-v3 behavior where internal routes bypass middleware,
    /// useful when health probes (Kubernetes, load balancers) cannot send auth headers.
    #[must_use]
    pub fn skip_middleware_for_internal(mut self, skip: bool) -> Self {
        self.skip_middleware_for_internal = skip;
        self
    }

    /// Register async middleware that wraps all route handlers.
    ///
    /// Middleware registered first executes outermost (wraps all later
    /// middleware). Each middleware receives the request and a `Next`
    /// handle — call `next.call(req).await` to continue the chain.
    ///
    /// ```ignore
    /// router.use_middleware(|req, next| async move {
    ///     let resp = next.call(req).await;
    ///     resp.with_header("X-Custom", "value")
    /// });
    /// ```
    pub fn use_middleware<F, Fut>(&mut self, mw: F)
    where
        F: Fn(&Request, super::middleware::Next) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send + 'static,
    {
        self.middleware
            .push(Box::new(move |req, next| Box::pin(mw(req, next))));
    }

    /// Register a GET handler for `path`.
    ///
    /// Path segments beginning with `:` are captured as named parameters.
    pub fn get<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Get, path, handler);
    }

    /// Register a POST handler for `path`.
    pub fn post<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Post, path, handler);
    }

    /// Register a PUT handler for `path`.
    pub fn put<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Put, path, handler);
    }

    /// Register a DELETE handler for `path`.
    pub fn delete<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Delete, path, handler);
    }

    /// Register a PATCH handler for `path`.
    pub fn patch<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Patch, path, handler);
    }

    /// Register a HEAD handler for `path`.
    ///
    /// If you do not register one, Camber can still answer HEAD requests for
    /// matching GET routes by stripping the response body automatically.
    pub fn head<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Head, path, handler);
    }

    /// Register an OPTIONS handler for `path`.
    pub fn options<F, Fut, R>(&mut self, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.add(Method::Options, path, handler);
    }

    /// Register an async streaming handler for GET requests.
    ///
    /// The handler returns a `StreamResponse` for incremental body delivery.
    /// Use `StreamResponse::new()` to get both the response and a sender.
    pub fn get_stream(
        &mut self,
        path: &str,
        handler: impl Fn(&Request) -> Pin<Box<dyn Future<Output = StreamResponse> + Send>>
        + Send
        + Sync
        + 'static,
    ) {
        self.add_stream(Method::Get, path, handler);
    }

    /// Register an async streaming handler for POST requests.
    pub fn post_stream(
        &mut self,
        path: &str,
        handler: impl Fn(&Request) -> Pin<Box<dyn Future<Output = StreamResponse> + Send>>
        + Send
        + Sync
        + 'static,
    ) {
        self.add_stream(Method::Post, path, handler);
    }

    /// Register an SSE streaming handler for GET requests.
    ///
    /// The handler receives the request and an `SseWriter` for sending events.
    /// The connection stays open until the handler returns or the client disconnects.
    pub fn get_sse(
        &mut self,
        path: &str,
        handler: impl Fn(&Request, &mut SseWriter) -> Result<(), RuntimeError> + Send + Sync + 'static,
    ) {
        self.root
            .insert_route(Method::Get, path, RouteHandler::Sse(Arc::new(handler)));
    }

    /// Register a WebSocket handler for the given path.
    ///
    /// The handler receives the upgrade request and a bidirectional `WsConn`.
    /// The connection stays open until the handler returns or the client disconnects.
    #[cfg(feature = "ws")]
    pub fn ws(
        &mut self,
        path: &str,
        handler: impl Fn(&Request, WsConn) -> Result<(), RuntimeError> + Send + Sync + 'static,
    ) {
        self.root.insert_route(
            Method::Get,
            path,
            RouteHandler::WebSocket(Arc::new(handler)),
        );
    }

    /// Register a gRPC service (generated by `camber-build`).
    ///
    /// Requests with `content-type: application/grpc` are forwarded to the
    /// tonic service. All other requests go through normal HTTP routing.
    #[cfg(feature = "grpc")]
    pub fn grpc(&mut self, grpc_router: super::dispatch::GrpcRouter) {
        self.grpc_router = Some(grpc_router);
    }

    /// Register a reverse proxy that forwards requests under `prefix` to `backend`.
    ///
    /// The prefix is stripped from the request path before forwarding.
    /// All HTTP methods are handled. The full upstream response is buffered,
    /// so middleware can inspect and modify the response body.
    /// On backend failure, returns 502.
    pub fn proxy(&mut self, prefix: &str, backend: &str) {
        self.insert_proxy_routes(prefix, backend, None, false);
    }

    /// Register a health-checked reverse proxy.
    ///
    /// Behaves like `proxy()` but checks the `healthy` flag before forwarding.
    /// When `healthy` is `false`, returns 503 immediately.
    pub fn proxy_checked(&mut self, prefix: &str, backend: &str, healthy: Arc<AtomicBool>) {
        self.insert_proxy_routes(prefix, backend, Some(healthy), false);
    }

    /// Register a streaming reverse proxy under `prefix`.
    ///
    /// Like `proxy()`, but the upstream response body is forwarded chunk-by-chunk
    /// with backpressure instead of being buffered in memory. Middleware acts as
    /// a request gate only — it can reject before the upstream call, but does not
    /// wrap the streamed response.
    pub fn proxy_stream(&mut self, prefix: &str, backend: &str) {
        self.insert_proxy_routes(prefix, backend, None, true);
    }

    /// Register a health-checked streaming reverse proxy.
    ///
    /// Behaves like `proxy_stream()` but checks the `healthy` flag before forwarding.
    /// When `healthy` is `false`, returns 503 immediately.
    pub fn proxy_checked_stream(&mut self, prefix: &str, backend: &str, healthy: Arc<AtomicBool>) {
        self.insert_proxy_routes(prefix, backend, Some(healthy), true);
    }

    fn insert_proxy_routes(
        &mut self,
        prefix: &str,
        backend: &str,
        healthy: Option<Arc<AtomicBool>>,
        streaming: bool,
    ) {
        let backend: Arc<str> = backend.into();
        let prefix_owned: Arc<str> = prefix.into();
        let wildcard_pattern = format!("{prefix}/*proxy_path");
        let exact_pattern = match prefix.is_empty() {
            true => "/".to_owned(),
            false => prefix.to_owned(),
        };

        let methods = [
            Method::Get,
            Method::Post,
            Method::Put,
            Method::Delete,
            Method::Patch,
            Method::Head,
            Method::Options,
        ];
        for method in methods {
            for pattern in [wildcard_pattern.as_str(), exact_pattern.as_str()] {
                let handler = proxy_route_handler(
                    streaming,
                    Arc::clone(&backend),
                    Arc::clone(&prefix_owned),
                    healthy.as_ref().map(Arc::clone),
                );
                self.root.insert_route(method, pattern, handler);
            }
        }
    }

    /// Serve static files from `dir` under the given URL `prefix`.
    pub fn static_files(&mut self, prefix: &str, dir: &str) {
        let exact_base_dir: Box<std::path::Path> = std::path::PathBuf::from(dir).into_boxed_path();
        let wildcard_base_dir: Box<std::path::Path> =
            std::path::PathBuf::from(dir).into_boxed_path();
        let wildcard_pattern = format!("{prefix}/*filepath");
        let exact_pattern = match prefix.is_empty() {
            true => "/".to_owned(),
            false => prefix.to_owned(),
        };
        self.root.insert_route(
            Method::Get,
            &exact_pattern,
            RouteHandler::Async(Box::new(move |_req: &Request| {
                let resp = super::static_files::serve_file(&exact_base_dir, "index.html");
                Box::pin(async move { resp }) as Pin<Box<dyn Future<Output = Response> + Send>>
            })),
        );
        self.root.insert_route(
            Method::Get,
            &wildcard_pattern,
            RouteHandler::Async(Box::new(move |req: &Request| {
                let file_path = req.param("filepath").unwrap_or("");
                let resp = super::static_files::serve_file(&wildcard_base_dir, file_path);
                Box::pin(async move { resp }) as Pin<Box<dyn Future<Output = Response> + Send>>
            })),
        );
    }

    fn add<F, Fut, R>(&mut self, method: Method, path: &str, handler: F)
    where
        F: Fn(&Request) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.root.insert_route(
            method,
            path,
            RouteHandler::Async(Box::new(move |req: &Request| {
                let fut = handler(req);
                Box::pin(async move { fut.await.into_response() })
                    as Pin<Box<dyn Future<Output = Response> + Send>>
            })),
        );
    }

    fn add_stream(
        &mut self,
        method: Method,
        path: &str,
        handler: impl Fn(&Request) -> Pin<Box<dyn Future<Output = StreamResponse> + Send>>
        + Send
        + Sync
        + 'static,
    ) {
        self.root
            .insert_route(method, path, RouteHandler::Stream(Box::new(handler)));
    }

    /// Freeze routes into an immutable trie for serving.
    pub(super) fn freeze(self) -> FrozenRouter {
        FrozenRouter {
            root: self.root.freeze(),
            middleware: self.middleware.into_boxed_slice(),
            skip_middleware_for_internal: self.skip_middleware_for_internal,
            #[cfg(feature = "grpc")]
            grpc_router: self.grpc_router,
        }
    }
}

fn proxy_route_handler(
    streaming: bool,
    backend: Arc<str>,
    prefix: Arc<str>,
    healthy: Option<Arc<std::sync::atomic::AtomicBool>>,
) -> RouteHandler {
    match streaming {
        true => RouteHandler::ProxyStream {
            backend,
            prefix,
            healthy,
        },
        false => RouteHandler::Proxy {
            backend,
            prefix,
            healthy,
        },
    }
}
