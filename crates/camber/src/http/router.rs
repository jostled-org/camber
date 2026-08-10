use super::body_admission::{
    BodyAdmission, BodyAdmissionContext, BodyPolicy, ConfiguredCeiling, shared_policy,
};
use super::method::Method;
use super::middleware::MiddlewareFn;
use super::rejection::{Rejection, RejectionContext, RejectionMapper, shared_mapper};
use super::response::IntoResponse;
use super::sse::SseWriter;
use super::stream::StreamResponse;
use super::trie::{CANONICAL_METHODS, HandlerOutcome, RouteHandler, TrieNode};
#[cfg(feature = "ws")]
use super::websocket::WsConn;
use super::{Request, Response};
use crate::RuntimeError;
use std::borrow::Cow;
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
            .field("has_rejection_mapper", &self.mapper.is_some())
            .field("body_ceiling", &self.body_ceiling)
            .field("has_body_admission", &self.body_policy.is_some())
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
    mapper: Option<Arc<RejectionMapper>>,
    body_ceiling: ConfiguredCeiling,
    body_policy: Option<Arc<BodyPolicy>>,
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
            mapper: None,
            body_ceiling: ConfiguredCeiling::default(),
            body_policy: None,
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
    ///
    /// This is a ceiling, not a target. A body-admission policy may select a
    /// smaller maximum for one request; it can never select a larger one, and
    /// under a [`HostRouter`](super::HostRouter) this ceiling can only narrow
    /// the host's.
    #[must_use]
    pub fn max_request_body(mut self, bytes: usize) -> Self {
        self.body_ceiling = ConfiguredCeiling::configured(bytes);
        self
    }

    /// Decide each body-consuming request's admission before its body is read.
    ///
    /// The policy runs once per matched body-consuming route — buffered routes
    /// and streaming proxy routes alike — before Camber polls a single payload
    /// frame. It is synchronous: it sees what the request head established and
    /// answers with the maximum it selects, plus an optional permit Camber
    /// holds and releases exactly once, at the point its mode names. A buffered
    /// route releases the permit when the request holding it is released; a
    /// streaming proxy route releases it when the upload ends.
    ///
    /// Returning `Err` refuses the request as `BodyAdmission` — `503` by
    /// default, with the error kept as a private diagnostic. A panic becomes a
    /// redacted `500`. Neither re-enters the policy, and neither reads a body
    /// byte or reaches the handler.
    ///
    /// Routes that consume no body — WebSocket upgrades, SSE, gRPC, internal
    /// routes, and routing terminals — never reach it.
    ///
    /// ```rust
    /// use camber::http::{BodyAdmission, BodyAdmissionContext, Router};
    /// use camber::RuntimeError;
    ///
    /// let router = Router::new().body_admission(|context: &BodyAdmissionContext<'_>| {
    ///     match context.header("x-upload-token") {
    ///         Some(_) => Ok(BodyAdmission::new(64 * 1024)),
    ///         None => Err(RuntimeError::InvalidArgument("upload token required".into())),
    ///     }
    /// });
    /// ```
    #[must_use]
    pub fn body_admission<F>(mut self, policy: F) -> Self
    where
        F: Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError>
            + Send
            + Sync
            + 'static,
    {
        self.body_policy = Some(shared_policy(policy));
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

    /// Set the policy that turns a Camber-controlled refusal into a response.
    ///
    /// The mapper is given the client-safe [`Rejection`] and the
    /// [`RejectionContext`] that names the request. It cannot reach the private
    /// cause, and it runs at most once per refusal. Returning `Err`, panicking,
    /// or answering with an informational status produces the fixed redacted
    /// `500` instead, without calling the mapper again.
    ///
    /// ```rust
    /// use camber::http::{Rejection, RejectionContext, Response, Router};
    ///
    /// let router = Router::new().rejection_mapper(|rejection: &Rejection, _: &RejectionContext| {
    ///     Response::text(rejection.status(), rejection.message())
    /// });
    /// ```
    #[must_use]
    pub fn rejection_mapper<F>(mut self, mapper: F) -> Self
    where
        F: Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError>
            + Send
            + Sync
            + 'static,
    {
        self.mapper = Some(shared_mapper(mapper));
        self
    }

    /// Register async middleware that wraps all route handlers.
    ///
    /// Middleware registered first executes outermost (wraps all later
    /// middleware). Each middleware receives the request and a `Next`
    /// handle — call `next.call(req).await` to continue the chain.
    ///
    /// A frame may answer with a `Response` or with
    /// `Result<Response, RuntimeError>`, so a failure it raises reaches the
    /// router's rejection policy instead of becoming a response nothing can
    /// classify.
    ///
    /// ```ignore
    /// router.use_middleware(|req, next| async move {
    ///     let resp = next.call(req).await;
    ///     resp.with_header("X-Custom", "value")
    /// });
    /// ```
    pub fn use_middleware<F, Fut, R>(&mut self, mw: F)
    where
        F: Fn(&Request, super::middleware::Next) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        self.middleware.push(Box::new(move |req, next| {
            let entered = mw(req, next);
            Box::pin(async move { entered.await.into_response() })
        }));
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
        let patterns = PrefixPatterns::under(prefix, "proxy_path");
        // A proxy forwards the same way under both patterns, so one builder
        // answers for both: the pair differs in what it matches, not in what it
        // does with the match.
        let forward = || {
            proxy_route_handler(
                streaming,
                Arc::clone(&backend),
                Arc::clone(&prefix_owned),
                healthy.as_ref().map(Arc::clone),
            )
        };

        for method in CANONICAL_METHODS {
            self.mount_prefix(&patterns, method, forward(), forward());
        }
    }

    /// Register one method under both patterns a prefix claims.
    ///
    /// The pair is the whole rule, and both prefix-mounted features owe it.
    /// What each pattern answers with is theirs: `beneath` answers everything
    /// under the prefix, and `at_prefix` answers the prefix itself.
    fn mount_prefix(
        &mut self,
        patterns: &PrefixPatterns,
        method: Method,
        beneath: RouteHandler,
        at_prefix: RouteHandler,
    ) {
        self.root.insert_route(method, &patterns.wildcard, beneath);
        self.root.insert_route(method, &patterns.exact, at_prefix);
    }

    /// Serve static files from `dir` under the given URL `prefix`.
    pub fn static_files(&mut self, prefix: &str, dir: &str) {
        let base_dir: Arc<std::path::Path> = Arc::from(std::path::PathBuf::from(dir));
        let patterns = PrefixPatterns::under(prefix, "filepath");
        self.mount_prefix(
            &patterns,
            Method::Get,
            static_route_handler(Arc::clone(&base_dir), |req| {
                Cow::from(req.param("filepath").unwrap_or("").to_owned())
            }),
            static_route_handler(base_dir, |_| Cow::Borrowed("index.html")),
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
                    as Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
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
            mapper: self.mapper,
            body_ceiling: self.body_ceiling,
            body_policy: self.body_policy,
            #[cfg(feature = "grpc")]
            grpc_router: self.grpc_router,
        }
    }
}

/// The two patterns one URL prefix registers.
///
/// Every prefix-mounted feature claims both: the prefix itself and everything
/// beneath it. Proxy registration and static-file serving derived the same pair
/// from the same prefix, differing only in the name they capture the tail
/// under, and two copies of the empty-prefix rule are two places `""` can stop
/// meaning the root.
struct PrefixPatterns {
    /// Everything beneath the prefix, captured under the given name.
    wildcard: Box<str>,
    /// The prefix itself. An empty prefix is the root.
    exact: Box<str>,
}

impl PrefixPatterns {
    fn under(prefix: &str, capture: &str) -> Self {
        Self {
            wildcard: format!("{prefix}/*{capture}").into_boxed_str(),
            exact: match prefix.is_empty() {
                true => "/".into(),
                false => prefix.into(),
            },
        }
    }
}

/// The file one static-files pattern serves, named by `select`.
///
/// The bare prefix is a directory request, which is answered with its
/// `index.html`; everything beneath it names its own file through the captured
/// tail. A [`Cow`] because only the tail is a value the request carries: the
/// index name is a constant borrowed for the life of the process, so serving it
/// costs no allocation to say what it is.
fn static_route_handler(
    base_dir: Arc<std::path::Path>,
    select: impl Fn(&Request) -> Cow<'static, str> + Send + Sync + 'static,
) -> RouteHandler {
    RouteHandler::Async(Box::new(move |req: &Request| {
        let base_dir = Arc::clone(&base_dir);
        let file_path = select(req);
        Box::pin(
            async move { Ok(super::static_files::serve_file_async(&base_dir, &file_path).await) },
        ) as Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
    }))
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
