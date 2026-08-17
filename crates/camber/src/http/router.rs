use super::body_admission::{
    BodyAdmission, BodyAdmissionContext, BodyPolicy, ConfiguredCeiling, shared_policy,
};
use super::method::Method;
use super::middleware::MiddlewareFn;
use super::multipart::{MultipartLimits, MultipartStream};
use super::proxy_policy::{ProxyPolicy, frozen_buffered_response_limit};
use super::rejection::{Rejection, RejectionContext, RejectionMapper, shared_mapper};
use super::response::HandlerOutcome;
use super::response::IntoResponse;
use super::sse::SseWriter;
use super::static_files::{DEFAULT_STATIC_FILE_LIMIT, frozen_static_file_limit};
use super::stream::StreamResponse;
use super::trie::{CANONICAL_METHODS, MultipartRegistration, RouteHandler, TrieNode};
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
    budgets: super::route_budgets::RouteBudgets,
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
            budgets: super::route_budgets::RouteBudgets::default(),
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

    /// Set the deadlines every request this router admits runs under.
    ///
    /// This narrows the server's own request budget and can never widen it: an
    /// unbounded dimension here inherits the containing bound rather than
    /// erasing it. Under a [`HostRouter`](super::HostRouter) the host's budget
    /// contains this one as well.
    #[must_use]
    pub fn request_budget(mut self, budget: super::RequestBudget) -> Self {
        self.budgets = self.budgets.with_request(budget);
        self
    }

    /// Set the budget for streaming uploads this router admits.
    ///
    /// Route-aware body admission remains the request payload byte authority;
    /// this budget can only narrow it further.
    #[must_use]
    pub fn upload_budget(mut self, budget: super::TransferBudget) -> Self {
        self.budgets = self.budgets.with_upload(budget);
        self
    }

    /// Set the budget for streaming downloads this router produces.
    #[must_use]
    pub fn download_budget(mut self, budget: super::TransferBudget) -> Self {
        self.budgets = self.budgets.with_download(budget);
        self
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

    /// Register a streaming multipart handler for `path` under `method`.
    ///
    /// The handler is given the metadata-only request — method, path, headers,
    /// route parameters, peer identity, TLS state, request id, and
    /// [`Request::on_disconnect`](super::Request::on_disconnect) — and one
    /// [`MultipartStream`], which is the whole body-access capability. Fields
    /// arrive in wire order, one at a time, under `limits`; each chunk is at
    /// most `max_chunk_bytes` and owns its own allocation.
    ///
    /// The total a peer may send is not configured here.
    /// [`Router::max_request_body`], [`HostRouter::max_request_body`](super::HostRouter::max_request_body),
    /// and [`Router::body_admission`] already resolve one effective maximum
    /// before the first payload frame is polled, and `limits` bounds the
    /// structure inside it.
    ///
    /// A `GET` registration does not also answer `HEAD`. Every other route kind
    /// answers a `HEAD` from its `GET` handler; this one reads a payload a
    /// `HEAD` request never sends, so the route names `GET` alone in its
    /// `Allow` value and refuses `HEAD` with `405 Method Not Allowed` instead
    /// of opening a session over an empty body.
    ///
    /// Admission and this router's middleware both run before any payload is
    /// read, and a missing or malformed request boundary is refused before the
    /// handler is reached. A handler must read to the end — until `next_field`
    /// answers `None` — for its response to be committed; dropping a field
    /// before its data ends is refused as an incomplete multipart body, and
    /// [`MultipartField::discard`](super::MultipartField::discard) is the one
    /// skip that succeeds.
    ///
    /// ```rust
    /// use camber::http::{Method, MultipartLimits, MultipartStream, Request, Response, Router};
    ///
    /// let mut router = Router::new();
    /// router.multipart(
    ///     Method::Post,
    ///     "/uploads/:id",
    ///     MultipartLimits::default(),
    ///     |request: &Request, mut stream: MultipartStream| {
    ///         let id: Box<str> = request.param("id").unwrap_or_default().into();
    ///         async move {
    ///             let mut received = 0;
    ///             while let Some(mut field) = stream.next_field().await? {
    ///                 while let Some(chunk) = field.next_chunk().await? {
    ///                     received += chunk.len();
    ///                 }
    ///             }
    ///             Response::text(200, &format!("{id} received {received} bytes"))
    ///         }
    ///     },
    /// );
    /// ```
    pub fn multipart<F, Fut, R>(
        &mut self,
        method: Method,
        path: &str,
        limits: MultipartLimits,
        handler: F,
    ) where
        F: Fn(&Request, MultipartStream) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R: IntoResponse + 'static,
    {
        let registration = MultipartRegistration::new(
            Box::new(move |req: &Request, stream: MultipartStream| {
                let entered = handler(req, stream);
                Box::pin(async move { entered.await.into_response() })
                    as Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
            }),
            limits,
        );
        self.root.insert_route(
            method,
            path,
            RouteHandler::Multipart(Arc::new(registration)),
        );
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
    /// The handler is synchronous and runs on the blocking pool. It receives the
    /// upgrade request and a [`WsConn`], the compatibility facade over the
    /// connection's two real owners.
    ///
    /// # Ownership
    ///
    /// [`WsConn::sender`] hands out a `Clone + Send + Sync` send handle without
    /// giving up the receive owner; [`WsConn::split`] gives up the facade for
    /// both. Both halves keep the connection live, so a handler that moves them
    /// into owned application work may return without ending it. A handler that
    /// drops the connection, or the last of its halves, ends it.
    ///
    /// # Blocking
    ///
    /// Sends and receives block. Each asks the connection before it asks the
    /// runtime, so one that has already ended answers with its cause on every
    /// flavor. A call that still has to wait waits on the calling thread off a
    /// runtime, waits through `block_in_place` on a multi-thread Tokio runtime,
    /// and returns [`RuntimeError::BlockingInAsyncContext`] on a current-thread
    /// runtime rather than wait.
    ///
    /// [`WsReceiver::recv_timeout`](crate::http::WsReceiver::recv_timeout) needs
    /// a Tokio clock for its deadline. Off a runtime it reports
    /// [`RuntimeError::NoRuntime`](crate::RuntimeError::NoRuntime) instead of
    /// waiting untimed, and an expired deadline reports
    /// [`RuntimeError::Timeout`](crate::RuntimeError::Timeout).
    ///
    /// A successful send means the frame entered the connection's bounded
    /// outbound queue, not that its bytes reached the peer. Once the connection
    /// ends, every send reports
    /// [`RuntimeError::WebSocketClosed`](crate::RuntimeError::WebSocketClosed)
    /// with the one cause both halves read — through the facade, that cause
    /// stays the broken pipe it has always been.
    ///
    /// # Runtime authority
    ///
    /// A handler served under a Camber runtime may admit work with
    /// [`camber::spawn`](crate::spawn); that child belongs to runtime
    /// completion, and a spawn issued after root admission closes is refused
    /// with [`RuntimeError::ScopeClosed`]. Synchronous serving is such a path —
    /// it carries the runtime its terminal call captured — as is an owned server
    /// started inside a Camber runtime. Only a bare-Tokio owned server refuses
    /// with [`RuntimeError::NoRuntime`], and a refused closure never runs. The
    /// handler itself is never a root-scope child, and server completion makes
    /// no claim that it has returned.
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
    ///
    /// The route buffers under [`ProxyPolicy`]'s default maximum of eight MiB.
    /// Use [`Router::proxy_with_policy`] to name another one, or to opt out of
    /// the maximum by name.
    pub fn proxy(&mut self, prefix: &str, backend: &str) {
        self.proxy_with_policy(prefix, backend, ProxyPolicy::default());
    }

    /// Register a buffered reverse proxy under the policy `policy` names.
    ///
    /// The buffered maximum is frozen with the route, so two routes to one
    /// backend keep the maximum each of them chose. An upstream answer above
    /// that maximum is refused as a bad gateway: the crossing data frame is
    /// never retained, no part of the upstream payload reaches the peer, and
    /// the bound that was crossed is recorded for the operator.
    pub fn proxy_with_policy(&mut self, prefix: &str, backend: &str, policy: ProxyPolicy) {
        self.insert_proxy_routes(prefix, backend, None, ProxyRegistration::buffered(policy));
    }

    /// Register a health-checked reverse proxy.
    ///
    /// Behaves like `proxy()` but checks the `healthy` flag before forwarding.
    /// When `healthy` is `false`, returns 503 immediately.
    pub fn proxy_checked(&mut self, prefix: &str, backend: &str, healthy: Arc<AtomicBool>) {
        self.proxy_checked_with_policy(prefix, backend, healthy, ProxyPolicy::default());
    }

    /// Register a health-checked buffered reverse proxy under `policy`.
    ///
    /// [`Router::proxy_with_policy`]'s health-checked form: the frozen maximum
    /// and its refusal are the same, and an unhealthy backend is refused with
    /// 503 before the upstream is reached at all.
    pub fn proxy_checked_with_policy(
        &mut self,
        prefix: &str,
        backend: &str,
        healthy: Arc<AtomicBool>,
        policy: ProxyPolicy,
    ) {
        self.insert_proxy_routes(
            prefix,
            backend,
            Some(healthy),
            ProxyRegistration::buffered(policy),
        );
    }

    /// Register a streaming reverse proxy under `prefix`.
    ///
    /// Like `proxy()`, but the upstream response body is forwarded chunk-by-chunk
    /// with backpressure instead of being buffered in memory. Middleware acts as
    /// a request gate only — it can reject before the upstream call, but does not
    /// wrap the streamed response.
    pub fn proxy_stream(&mut self, prefix: &str, backend: &str) {
        self.insert_proxy_routes(prefix, backend, None, ProxyRegistration::Streaming);
    }

    /// Register a health-checked streaming reverse proxy.
    ///
    /// Behaves like `proxy_stream()` but checks the `healthy` flag before forwarding.
    /// When `healthy` is `false`, returns 503 immediately.
    pub fn proxy_checked_stream(&mut self, prefix: &str, backend: &str, healthy: Arc<AtomicBool>) {
        self.insert_proxy_routes(prefix, backend, Some(healthy), ProxyRegistration::Streaming);
    }

    fn insert_proxy_routes(
        &mut self,
        prefix: &str,
        backend: &str,
        healthy: Option<Arc<AtomicBool>>,
        registration: ProxyRegistration,
    ) {
        let backend: Arc<str> = backend.into();
        let prefix_owned: Arc<str> = prefix.into();
        let patterns = PrefixPatterns::under(prefix, "proxy_path");
        // A proxy forwards the same way under both patterns, so one builder
        // answers for both: the pair differs in what it matches, not in what it
        // does with the match.
        let forward = || {
            registration.route_handler(
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
    ///
    /// Each file is read under the documented eight-MiB default maximum. A file
    /// past it is refused as an internal service failure carrying
    /// [`ByteBoundary::StaticFile`](super::ByteBoundary::StaticFile), because a
    /// served root holding content the service cannot answer with is the
    /// operator's configuration and not the peer's request.
    pub fn static_files(&mut self, prefix: &str, dir: &str) {
        self.mount_static(prefix, dir, Some(DEFAULT_STATIC_FILE_LIMIT));
    }

    /// Serve static files under a maximum of exactly `max_bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] naming `max_bytes` when the
    /// stated maximum is zero. The route is not registered, so no request can
    /// reach the filesystem under a maximum nothing accepted.
    pub fn static_files_with_limit(
        &mut self,
        prefix: &str,
        dir: &str,
        max_bytes: usize,
    ) -> Result<(), RuntimeError> {
        self.mount_static(prefix, dir, frozen_static_file_limit(max_bytes)?);
        Ok(())
    }

    /// Serve static files with no maximum at all.
    ///
    /// The explicit opt-out, and the only routed spelling that removes the
    /// ceiling. Every matched file is retained in memory whatever its size, so
    /// this belongs to a root the operator controls. A root anything untrusted
    /// can write to makes the file's author the author of this process's
    /// memory use.
    pub fn static_files_unbounded(&mut self, prefix: &str, dir: &str) {
        self.mount_static(prefix, dir, None);
    }

    /// Mount one static root under both patterns its prefix claims.
    ///
    /// The three public spellings differ only in the maximum they froze, so the
    /// registration itself is stated once: a second copy is a second place the
    /// index-at-prefix rule and the captured tail can drift.
    fn mount_static(&mut self, prefix: &str, dir: &str, limit: Option<usize>) {
        let base_dir: Arc<std::path::Path> = Arc::from(std::path::PathBuf::from(dir));
        let patterns = PrefixPatterns::under(prefix, "filepath");
        self.mount_prefix(
            &patterns,
            Method::Get,
            static_route_handler(Arc::clone(&base_dir), limit, |req| {
                Cow::from(req.param("filepath").unwrap_or("").to_owned())
            }),
            static_route_handler(base_dir, limit, |_| Cow::Borrowed("index.html")),
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
            budgets: self.budgets,
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
    limit: Option<usize>,
    select: impl Fn(&Request) -> Cow<'static, str> + Send + Sync + 'static,
) -> RouteHandler {
    RouteHandler::Async(Box::new(move |req: &Request| {
        let base_dir = Arc::clone(&base_dir);
        let file_path = select(req);
        Box::pin(
            async move { super::static_files::read_bounded(&base_dir, &file_path, limit).await },
        ) as Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
    }))
}

/// How one registered proxy route carries its answer back.
///
/// The buffered kind names the maximum it collects under; the streaming kind
/// retains nothing to bound. Stated as the two kinds rather than as a flag and
/// an ignored maximum, so a registration that can name a ceiling and one that
/// cannot are told apart by the type the caller built.
enum ProxyRegistration {
    /// Buffered, under the maximum this route froze.
    Buffered { buffered_limit: Option<usize> },
    /// Streamed to the peer with backpressure.
    Streaming,
}

impl ProxyRegistration {
    /// Freeze the buffered maximum `policy` names.
    fn buffered(policy: ProxyPolicy) -> Self {
        Self::Buffered {
            buffered_limit: frozen_buffered_response_limit(&policy),
        }
    }

    /// The route this registration mounts under one pattern.
    fn route_handler(
        &self,
        backend: Arc<str>,
        prefix: Arc<str>,
        healthy: Option<Arc<AtomicBool>>,
    ) -> RouteHandler {
        match *self {
            Self::Buffered { buffered_limit } => RouteHandler::Proxy {
                backend,
                prefix,
                healthy,
                buffered_limit,
            },
            Self::Streaming => RouteHandler::ProxyStream {
                backend,
                prefix,
                healthy,
            },
        }
    }
}
