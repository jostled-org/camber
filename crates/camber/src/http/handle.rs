use super::body::HyperResponseBody;
use super::disconnect::DisconnectSignal;
use super::dispatch::{AsyncDispatch, FrozenRouter, RouteClass, Routed};
#[cfg(feature = "profiling")]
use super::internal_routes::match_profiling_route;
use super::internal_routes::{
    build_internal_handler, invoke_internal_route, match_internal_route_from_path,
};
use super::record::record_request;
use super::request::{RequestHead, RequestOrigin, method_is_head};
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
use std::sync::Arc;
use std::time::Duration;

const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(feature = "grpc")]
use super::grpc_support::is_grpc_request;

pub(super) struct ConnCtx {
    pub(super) tracing_enabled: bool,
    pub(super) metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    #[cfg(feature = "profiling")]
    pub(super) profiling_enabled: bool,
    pub(super) max_request_body: usize,
    pub(super) sse_buffer_size: usize,
    #[cfg(feature = "ws")]
    pub(super) ws_buffer_size: usize,
    pub(super) health_state: Option<HealthState>,
    pub(super) is_tls: bool,
}

impl ConnCtx {
    /// Build from a running Camber runtime and buffer configuration.
    pub(super) fn from_runtime(
        rt: &Arc<RuntimeInner>,
        buffers: BufferConfig,
        is_tls: bool,
    ) -> Self {
        Self {
            tracing_enabled: rt.config.tracing_enabled,
            metrics_handle: rt.metrics_handle.clone(),
            #[cfg(feature = "profiling")]
            profiling_enabled: rt.config.profiling_enabled,
            max_request_body: buffers.max_request_body,
            sse_buffer_size: buffers.sse_buffer_size,
            #[cfg(feature = "ws")]
            ws_buffer_size: buffers.ws_buffer_size,
            health_state: rt.health_state.clone(),
            is_tls,
        }
    }
}

/// A request refused before it ever reached dispatch, and the identity to
/// record it under.
///
/// The response alone was not enough. Every pre-dispatch refusal left with no
/// method and no path to name, so nothing recorded it: a peer oversizing every
/// body, or sending an unnameable method on every request, could drive a server
/// to shed all of its traffic while `http_requests_total` and
/// `http_request_duration_seconds` stayed flat.
struct Refused {
    refusal: Response,
    /// `None` for a method Camber cannot name — the one refusal whose method
    /// has no name to record it under.
    method: Option<super::method::Method>,
    uri: hyper::Uri,
}

impl Refused {
    /// Refuse a head Camber cannot name, against the request that sent it.
    fn unnameable(uri: hyper::Uri) -> Box<Self> {
        Box::new(Self {
            refusal: method_not_allowed_response(),
            method: None,
            uri,
        })
    }
}

/// What a request-building step answers with.
///
/// The refusal is boxed, and boxed at EVERY step rather than at one of them: a
/// `Response` beside a `Uri` is ~192 bytes, over `clippy::result_large_err`'s
/// threshold, and that weight would otherwise ride the success path's stack on
/// every request to describe a refusal that usually never fires. Boxing one
/// step and not the next is what forces a frame to unbox and rebox, so all five
/// builders and the entry point that consumes them agree on the shape.
type Building = Result<DispatchInput, Box<Refused>>;

/// Collect the hyper body with a size limit and build a Camber Request.
///
/// `method` arrives already parsed, from the classification that read this
/// request's head. A method Camber cannot name is answered as `Unnameable`
/// before a body is read, so this path has one refusal rather than two, and the
/// URI moves into whichever of the two outcomes needs it instead of being
/// copied for a refusal that usually never fires.
async fn collect_body_limited(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    max_body: usize,
    origin: RequestOrigin<'_>,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
    method: super::method::Method,
) -> Result<Request, Box<Refused>> {
    let (parts, body) = hyper_req.into_parts();
    let body_bytes = match collect_body(body, max_body, lifecycle_script).await {
        Ok(bytes) => bytes,
        Err(refusal) => {
            return Err(Box::new(Refused {
                refusal,
                method: Some(method),
                uri: parts.uri,
            }));
        }
    };

    Ok(Request::from_hyper(parts, body_bytes, origin, method))
}

/// Read and size-limit a request body. Separate function to avoid nested match.
async fn collect_body(
    body: hyper::body::Incoming,
    max_body: usize,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
) -> Result<bytes::Bytes, Response> {
    use http_body_util::BodyExt;
    super::mock::LifecycleScript::pause_at(
        lifecycle_script,
        super::mock::LifecycleCheckpoint::RequestBodyLimitConfigured(max_body),
    )
    .await;
    let limited = http_body_util::Limited::new(body, max_body);
    match tokio::time::timeout(REQUEST_BODY_TIMEOUT, limited.collect()).await {
        Ok(Ok(collected)) => Ok(collected.to_bytes()),
        Ok(Err(_)) => Err(Response::text_raw(413, "request body too large")),
        Err(_) => Err(Response::text_raw(408, "request body timeout")),
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
) -> Building {
    let ws = ws_proxy::extract_ws_upgrade(&mut hyper_req);
    let head = RequestHead::from_hyper_request(&hyper_req, origin)
        .ok_or_else(|| Refused::unnameable(hyper_req.uri().clone()))?;
    Ok((head.to_request(None), ws))
}

/// Build a Request from head metadata with an empty body.
///
/// Used for head-only routes (SSE) that skip body collection.
#[cfg(not(feature = "ws"))]
fn build_head_only_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    origin: RequestOrigin<'_>,
) -> Building {
    let head = RequestHead::from_hyper_request(&hyper_req, origin)
        .ok_or_else(|| Refused::unnameable(hyper_req.uri().clone()))?;
    Ok(head.to_request(None))
}

/// Consume a hyper request into a Camber Request (body-limited, WS-extracted).
#[cfg(feature = "ws")]
async fn collect_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    max_body: usize,
    origin: RequestOrigin<'_>,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
    method: super::method::Method,
) -> Building {
    let mut r = hyper_req;
    let ws_upgrade = ws_proxy::extract_ws_upgrade(&mut r);
    let req = collect_body_limited(r, max_body, origin, lifecycle_script, method).await?;
    Ok((req, ws_upgrade))
}

/// Consume a hyper request into a Camber Request (body-limited).
#[cfg(not(feature = "ws"))]
async fn collect_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    max_body: usize,
    origin: RequestOrigin<'_>,
    lifecycle_script: Option<&super::mock::LifecycleScript>,
    method: super::method::Method,
) -> Building {
    collect_body_limited(hyper_req, max_body, origin, lifecycle_script, method).await
}

/// Read the wire the way the head classification decided it should be read.
///
/// Head-only and unmatched routes never expose a body to a handler, so
/// collecting one would buffer bytes no handler can reach. Both arms answer
/// with the refusal the request earned, so the entry point has one `?`-shaped
/// decision rather than one per build configuration.
async fn build_dispatch_input(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    lifecycle: &ConnectionLifecycle,
    skip_body: bool,
    method: super::method::Method,
) -> Building {
    match skip_body {
        true => build_head_only_request(hyper_req, origin),
        false => {
            let lifecycle_script = lifecycle.script();
            collect_request(
                hyper_req,
                ctx.max_request_body,
                origin,
                lifecycle_script.as_deref(),
                method,
            )
            .await
        }
    }
}

/// What the request head decides before a body is read.
enum PreBodyRoute {
    /// An internal route (`/health`, `/metrics`, `/debug/pprof/cpu`), which
    /// bypasses body collection entirely, and the method it was asked with.
    Internal(super::internal_routes::InternalRoute, super::method::Method),
    /// How the dispatch trie classifies the route, and the method it was
    /// classified under. The method travels with the class because every path
    /// below needs it to record what it answered, and parsing it a second time
    /// would only re-derive what the head already proved.
    Class(RouteClass, super::method::Method),
    /// A method Camber cannot name.
    ///
    /// It reaches no route, no handler, and no middleware, so the only answer
    /// it can get is `405`. Carried out of classification so the body is never
    /// read for one: routing it as buffered spent a full body read, and up to
    /// `max_request_body` of heap, on a request already known to be refused.
    Unnameable,
}

/// Classify a request from its head alone.
///
/// The borrow of `hyper_req` ends here, before any mutable access or body
/// collection. A head Camber cannot name — an unparseable method — leaves as
/// `Unnameable` rather than being routed on a guess.
fn classify_pre_body(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
) -> PreBodyRoute {
    let head = match RequestHead::from_hyper_request(hyper_req, origin) {
        Some(head) => head,
        None => return PreBodyRoute::Unnameable,
    };
    let internal = match_internal_route_from_path(head.path(), ctx);
    #[cfg(feature = "profiling")]
    let internal = internal.or_else(|| match_profiling_route(head.path(), head.raw_query(), ctx));
    match internal {
        Some(route) => PreBodyRoute::Internal(route, head.method()),
        None => PreBodyRoute::Class(dispatch.classify_route(&head), head.method()),
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
) -> Option<GateCheck> {
    match (result.needs_middleware_gate(), router) {
        (true, Some(router)) => router.middleware_gate(result.request_ref()),
        _ => None,
    }
}

/// Put a buffered response on the wire and record the status the peer is given.
///
/// The record is read off the CONVERTED response, never off the one handed in.
/// `Response::into_hyper` answers with its own `500` for a response it cannot
/// build — an unrepresentable header name is enough, and `with_header` does not
/// validate — so a metric taken before the conversion could name a status no
/// peer ever saw.
///
/// A HEAD keeps its headers and loses its body, stripped here rather than at
/// each caller: every buffered answer this file gives leaves through this
/// function, so none of them can strip a body another one keeps.
fn answer(
    ctx: &ConnCtx,
    method: &'static str,
    path: &str,
    is_head: bool,
    resp: Response,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let converted = to_hyper_full(strip_body_if_head(is_head, resp));
    record_request(ctx, method, path, converted.status().as_u16(), start);
    converted
}

/// Record a refusal against the request that produced it.
fn refuse(
    ctx: &ConnCtx,
    result: &DispatchResult,
    refusal: Response,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let req = result.request_ref();
    answer(ctx, req.method(), req.path(), req.is_head(), refusal, start)
}

/// The label a request whose method Camber cannot name is recorded under.
///
/// `record_request` names methods with `&'static str`, and an unnameable method
/// has no such name. Recording it under this keeps the refusal countable; not
/// recording it is what let a peer shed every request without moving a counter.
const UNNAMEABLE_METHOD: &str = "UNKNOWN";

/// Record a refusal decided from the head alone, before dispatch was reached.
///
/// Recorded like every other refusal this file answers with. The buffered
/// proxy's own unhealthy arm returns its `503` through `finish_async`, so it is
/// counted and logged; leaving the head refusals unrecorded made the classes
/// that shed traffic the ones no metric or request log could see.
pub(super) fn refuse_head(
    ctx: &ConnCtx,
    method: Option<super::method::Method>,
    path: &str,
    refusal: Response,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let is_head = method.is_some_and(method_is_head);
    let name = method.map_or(UNNAMEABLE_METHOD, super::method::Method::as_str);
    answer(ctx, name, path, is_head, refusal, start)
}

/// Shared facts needed after a request head has selected a dispatch route.
struct RequestDispatch<'a> {
    dispatch: &'a ServerDispatch,
    ctx: &'a ConnCtx,
    origin: RequestOrigin<'a>,
    lifecycle: &'a ConnectionLifecycle,
    start: std::time::Instant,
}

/// Dispatch a classified route without losing its body-collection contract.
async fn dispatch_classified_route(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    route_class: RouteClass,
    method: super::method::Method,
    request_dispatch: &RequestDispatch<'_>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &RequestDispatch {
        dispatch,
        ctx,
        origin,
        lifecycle,
        start,
    } = request_dispatch;
    #[cfg(feature = "ws")]
    let is_ws_upgrade = ws_proxy::is_ws_upgrade_head(hyper_req.headers());
    #[cfg(not(feature = "ws"))]
    let is_ws_upgrade = false;

    let skip_body_collection = matches!(route_class, RouteClass::HeadOnly | RouteClass::Unmatched)
        || (matches!(&route_class, RouteClass::StreamingProxy(_)) && is_ws_upgrade);
    match route_class {
        RouteClass::StreamingProxy(target) if !is_ws_upgrade => {
            return dispatch_streaming_proxy(
                hyper_req, dispatch, ctx, target, origin, method, start,
            )
            .await;
        }
        RouteClass::Refused(refusal) => {
            let path = hyper_req.uri().path();
            return Ok(refuse_head(ctx, Some(method), path, refusal, start));
        }
        // Listed rather than wildcarded, so a new `RouteClass` is a compile
        // error here instead of a silent fall-through into body collection.
        // `StreamingProxy(_)` appears because the guarded arm above it does not
        // cover the WS-upgrade case.
        RouteClass::HeadOnly
        | RouteClass::Unmatched
        | RouteClass::Buffered
        | RouteClass::StreamingProxy(_) => {}
    }

    let input = match build_dispatch_input(
        hyper_req,
        ctx,
        origin,
        lifecycle,
        skip_body_collection,
        method,
    )
    .await
    {
        Ok(input) => input,
        Err(refused) => {
            let Refused {
                refusal,
                method: refused_method,
                uri,
            } = *refused;
            return Ok(refuse_head(ctx, refused_method, uri.path(), refusal, start));
        }
    };
    dispatch_built_request(input, request_dispatch).await
}

/// Run middleware gates and finish a request whose wire representation is built.
async fn dispatch_built_request(
    input: DispatchInput,
    request_dispatch: &RequestDispatch<'_>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &RequestDispatch {
        dispatch,
        ctx,
        lifecycle,
        start,
        ..
    } = request_dispatch;
    #[cfg(feature = "ws")]
    let (req, ws_upgrade) = input;
    #[cfg(not(feature = "ws"))]
    let req = input;
    let Routed { result, router } = dispatch.dispatch(req);
    let gate_blocked = match pending_middleware_gate(&result, router) {
        None => None,
        Some(GateCheck { reached, fut }) => gate_result(reached, fut.await),
    };
    if let Some(blocked) = gate_blocked {
        return Ok(refuse(ctx, &result, blocked, start));
    }

    #[cfg(feature = "ws")]
    if let Some(rejected) = result
        .is_websocket()
        .then(|| ws_proxy::check_ws_origin(result.request_ref()))
        .flatten()
    {
        return Ok(refuse(ctx, &result, rejected, start));
    }

    match result {
        DispatchResult::Async(fut, req) => finish_async(ctx, &req, fut.await, start),
        DispatchResult::Stream(fut, req) => handle_stream_response(fut.await, req, ctx, start),
        DispatchResult::Sse(handler, req) => {
            record_request(ctx, req.method(), req.path(), 200, start);
            handle_sse(handler, req, ctx.sse_buffer_size, lifecycle).await
        }
        #[cfg(feature = "ws")]
        DispatchResult::WebSocket(handler, req) => {
            record_upgrade(ctx, req, start, |req| {
                ws_proxy::handle_ws_upgrade(ws_upgrade, handler, req, ctx.ws_buffer_size, lifecycle)
            })
            .await
        }
        #[cfg(feature = "ws")]
        DispatchResult::ProxyWebSocket(req, backend, prefix) => {
            record_upgrade(ctx, req, start, |req| {
                ws_proxy::handle_proxy_ws(ws_upgrade, req, backend, prefix, lifecycle)
            })
            .await
        }
        DispatchResult::ProxyStream(req, backend, prefix) => {
            handle_proxy_stream_response(req, &backend, &prefix, ctx, start).await
        }
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
    disconnect: DisconnectSignal,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let start = std::time::Instant::now();
    // Built once and copied down every dispatch path, so no path can pair this
    // peer with another request's lifetime signal.
    let origin = RequestOrigin {
        remote_addr,
        is_tls: ctx.is_tls,
        disconnect: &disconnect,
    };

    // gRPC bodies are streaming — skip body collection and dispatch directly to tonic.
    // Middleware runs as a gate check on the headers, then forwards to tonic.
    #[cfg(feature = "grpc")]
    let hyper_req = match try_dispatch_grpc(hyper_req, dispatch, ctx, origin, start).await {
        GrpcDispatch::Handled(resp) => return resp,
        GrpcDispatch::NotGrpc(req) => req,
    };

    let (route_class, method) = match classify_pre_body(&hyper_req, dispatch, ctx, origin) {
        // Internal routes (/health, /metrics, /debug/pprof/cpu) bypass body
        // collection.
        PreBodyRoute::Internal(route, method) => {
            return dispatch_internal_head_only(
                &hyper_req, route, dispatch, ctx, origin, method, start,
            )
            .await;
        }
        // Answered before the wire is read: nothing downstream could route it,
        // and collecting a body first would buffer megabytes for a `405`.
        PreBodyRoute::Unnameable => {
            return Ok(refuse_head(
                ctx,
                None,
                hyper_req.uri().path(),
                method_not_allowed_response(),
                start,
            ));
        }
        PreBodyRoute::Class(class, method) => (class, method),
    };
    let request_dispatch = RequestDispatch {
        dispatch,
        ctx,
        origin,
        lifecycle,
        start,
    };
    dispatch_classified_route(hyper_req, route_class, method, &request_dispatch).await
}

/// Finish a buffered dispatch against the request that produced it.
///
/// Both buffered entry points end here, and here ends in [`answer`], so neither
/// can strip a body the other keeps or record a status it did not answer with.
fn finish_async(
    ctx: &ConnCtx,
    req: &Request,
    resp: Response,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    Ok(answer(
        ctx,
        req.method(),
        req.path(),
        req.is_head(),
        resp,
        start,
    ))
}

/// Run an upgrade and record what it ANSWERED, not the `101` it hoped for.
///
/// A rejected handshake, a refused registrar, or an unbuildable response all
/// leave with their own status. The request's name is taken before it is handed
/// to the upgrade, because the request itself moves into the bridge — the URI is
/// `Bytes`-backed, so holding one is a refcount bump. Both upgrade kinds record
/// through here, so that invariant is stated once rather than per kind.
#[cfg(feature = "ws")]
async fn record_upgrade<F, Fut>(
    ctx: &ConnCtx,
    req: Request,
    start: std::time::Instant,
    upgrade: F,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible>
where
    F: FnOnce(Request) -> Fut,
    Fut: std::future::Future<Output = hyper::Response<HyperResponseBody>>,
{
    let (method, uri) = (req.method(), req.uri_owned());
    let resp = upgrade(req).await;
    record_request(ctx, method, uri.path(), resp.status().as_u16(), start);
    Ok(resp)
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
    method: super::method::Method,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    match dispatch.skip_middleware_for_internal() {
        true => Ok(answer(
            ctx,
            method.as_str(),
            hyper_req.uri().path(),
            method_is_head(method),
            invoke_internal_route(&route),
            start,
        )),
        false => {
            dispatch_internal_through_middleware(hyper_req, route, dispatch, ctx, origin, start)
                .await
        }
    }
}

/// Build a lightweight Request from head metadata and run the internal route
/// through the middleware chain.
async fn dispatch_internal_through_middleware(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    route: super::internal_routes::InternalRoute,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let head = match RequestHead::from_hyper_request(hyper_req, origin) {
        Some(h) => h,
        None => {
            return Ok(refuse_head(
                ctx,
                None,
                hyper_req.uri().path(),
                method_not_allowed_response(),
                start,
            ));
        }
    };
    let req = head.to_request(None);
    let handler = build_internal_handler(route);
    let AsyncDispatch { fut, req } = dispatch.dispatch_with_handler(&handler, req);
    finish_async(ctx, &req, fut.await, start)
}

pub(super) fn to_hyper_full(resp: Response) -> hyper::Response<HyperResponseBody> {
    let (parts, body) = resp.into_hyper().into_parts();
    hyper::Response::from_parts(parts, HyperResponseBody::Full(body))
}

fn strip_body_if_head(is_head: bool, resp: Response) -> Response {
    match is_head {
        true => resp.strip_body(),
        false => resp,
    }
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
    let is_grpc = dispatch.grpc_router().is_some() && is_grpc_request(&hyper_req);
    match is_grpc {
        false => GrpcDispatch::NotGrpc(hyper_req),
        true => GrpcDispatch::Handled(
            dispatch_grpc_inner(hyper_req, dispatch, ctx, origin, start).await,
        ),
    }
}

/// Run the middleware gate and dispatch to tonic. Called only when the request
/// is confirmed gRPC and a grpc_router exists.
#[cfg(feature = "grpc")]
async fn dispatch_grpc_inner(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    origin: RequestOrigin<'_>,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    // Named once for every refusal below. It is `None` exactly when the head
    // cannot be built, which is the one refusal `run_head_gate` reports as
    // `MethodNotAllowed`.
    let method = super::method::Method::from_hyper(hyper_req.method());
    let grpc_router = match dispatch.grpc_router() {
        Some(r) => r,
        None => {
            return Ok(refuse_head(
                ctx,
                method,
                hyper_req.uri().path(),
                Response::text_raw(500, "grpc router missing"),
                start,
            ));
        }
    };
    // Build a lightweight Request from headers for the middleware gate check.
    // The streaming gRPC body is preserved for tonic.
    match run_head_gate(&hyper_req, dispatch, origin, None).await {
        Err(MethodNotAllowed) => Ok(refuse_head(
            ctx,
            method,
            hyper_req.uri().path(),
            method_not_allowed_response(),
            start,
        )),
        Ok(Some(refusal)) => Ok(refuse_head(
            ctx,
            method,
            hyper_req.uri().path(),
            refusal,
            start,
        )),
        Ok(None) => {
            // Tonic owns the response body from here, so this handoff is
            // Camber's last observation of the request.
            origin.disconnect.complete();
            grpc_router.dispatch(hyper_req).await
        }
    }
}

/// A request whose method Camber cannot name.
///
/// Distinct from "the chain passed": a head no `RequestHead` could be built
/// from was never offered to middleware at all, so a caller that read it as a
/// pass would forward the request upstream with no gate ever run.
pub(super) struct MethodNotAllowed;

/// Run middleware as a gate check for a streaming request (gRPC, streaming proxy).
///
/// Borrows URI and HeaderMap from the hyper request via `RequestHead`.
/// Only clones into an owned `Request` when middleware actually exists.
/// Returns `Ok(Some(response))` if the request is refused, `Ok(None)` if
/// middleware passed, and `Err(MethodNotAllowed)` if the gate could not be
/// built at all.
///
/// A Host header no router can be resolved from is one of the refusals, not a
/// pass. This gate guards the gRPC and streaming-proxy paths, both of which
/// forward upstream on `Ok(None)`, so an unresolvable host reported as a pass
/// would reach the upstream with the chain never run.
pub(super) async fn run_head_gate(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    origin: RequestOrigin<'_>,
    params: Option<super::request::Params>,
) -> Result<Option<Response>, MethodNotAllowed> {
    let head = match RequestHead::from_hyper_request(hyper_req, origin) {
        Some(head) => head,
        None => return Err(MethodNotAllowed),
    };
    let GateCheck { reached, fut } = match dispatch.middleware_gate_head(&head, params) {
        Ok(Some(gate)) => gate,
        Ok(None) => return Ok(None),
        Err(refusal) => return Ok(Some(refusal)),
    };
    Ok(gate_result(reached, fut.await))
}

/// The answer to a method Camber cannot name.
///
/// Every path that meets one gives this same response, whether it is found at
/// body collection, at head construction, or at a gate that could not be built.
/// One constructor, so the refusal that is recorded and the refusal that goes
/// on the wire cannot drift apart.
pub(super) fn method_not_allowed_response() -> Response {
    Response::text_raw(405, "method not allowed")
}
