use super::body::{HyperResponseBody, StreamBody};
use super::dispatch::RouteClass;
#[cfg(feature = "profiling")]
use super::internal_routes::match_profiling_route;
use super::internal_routes::{
    build_internal_handler, invoke_internal_route, match_internal_route_from_path,
};
use super::request::RequestHead;
use super::router::{DispatchResult, GateCheck, ServerDispatch, gate_result};
use super::sse::SseWriter;
#[cfg(feature = "ws")]
use super::ws_proxy::{self, WsUpgrade};
use super::{BufferConfig, Request, Response};
use crate::resource::HealthState;
use crate::runtime_state::RuntimeInner;
use std::sync::Arc;

fn build_status_text_table() -> Box<[Box<str>]> {
    (100u16..600)
        .map(|code| Box::from(code.to_string()))
        .collect()
}

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

    /// Build without a Camber runtime (standalone async serving).
    pub(super) fn without_runtime(buffers: BufferConfig, is_tls: bool) -> Self {
        Self {
            tracing_enabled: false,
            metrics_handle: None,
            #[cfg(feature = "profiling")]
            profiling_enabled: false,
            max_request_body: buffers.max_request_body,
            sse_buffer_size: buffers.sse_buffer_size,
            #[cfg(feature = "ws")]
            ws_buffer_size: buffers.ws_buffer_size,
            health_state: None,
            is_tls,
        }
    }
}

/// Collect the hyper body with a size limit and build a Camber Request.
async fn collect_body_limited(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    max_body: usize,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<Request, hyper::Response<HyperResponseBody>> {
    let (parts, body) = hyper_req.into_parts();
    let body_bytes = collect_body(body, max_body).await?;

    let mut req = match Request::from_hyper(parts, body_bytes) {
        Some(r) => r,
        None => return Err(to_hyper_full(Response::text_raw(405, "method not allowed"))),
    };
    if let Some(addr) = remote_addr {
        req.set_remote_addr(addr);
    }
    req.set_tls(is_tls);

    Ok(req)
}

/// Read and size-limit a request body. Separate function to avoid nested match.
async fn collect_body(
    body: hyper::body::Incoming,
    max_body: usize,
) -> Result<bytes::Bytes, hyper::Response<HyperResponseBody>> {
    use http_body_util::BodyExt;
    let limited = http_body_util::Limited::new(body, max_body);
    match limited.collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(_) => Err(to_hyper_full(Response::text_raw(
            413,
            "request body too large",
        ))),
    }
}

/// Build a Request from head metadata with an empty body (WS-extracted).
///
/// Used for head-only routes (WebSocket, SSE) that skip body collection.
/// Error is boxed to avoid a large Result on the stack.
#[cfg(feature = "ws")]
fn build_head_only_request_ws(
    mut hyper_req: hyper::Request<hyper::body::Incoming>,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<(Request, WsUpgrade), Box<hyper::Response<HyperResponseBody>>> {
    let ws = ws_proxy::extract_ws_upgrade(&mut hyper_req);
    let head = RequestHead::from_hyper_request(&hyper_req, remote_addr, is_tls)
        .ok_or_else(|| Box::new(to_hyper_full(Response::text_raw(405, "method not allowed"))))?;
    Ok((head.to_request(None), ws))
}

/// Build a Request from head metadata with an empty body.
///
/// Used for head-only routes (SSE) that skip body collection.
/// Error is boxed to avoid a large Result on the stack.
#[cfg(not(feature = "ws"))]
fn build_head_only_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<Request, Box<hyper::Response<HyperResponseBody>>> {
    let head = RequestHead::from_hyper_request(&hyper_req, remote_addr, is_tls)
        .ok_or_else(|| Box::new(to_hyper_full(Response::text_raw(405, "method not allowed"))))?;
    Ok(head.to_request(None))
}

/// Consume a hyper request into a Camber Request (body-limited, WS-extracted).
#[cfg(feature = "ws")]
async fn collect_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    max_body: usize,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<(Request, WsUpgrade), hyper::Response<HyperResponseBody>> {
    let mut r = hyper_req;
    let ws_upgrade = ws_proxy::extract_ws_upgrade(&mut r);
    let req = collect_body_limited(r, max_body, remote_addr, is_tls).await?;
    Ok((req, ws_upgrade))
}

/// Consume a hyper request into a Camber Request (body-limited).
#[cfg(not(feature = "ws"))]
async fn collect_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    max_body: usize,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<Request, hyper::Response<HyperResponseBody>> {
    collect_body_limited(hyper_req, max_body, remote_addr, is_tls).await
}

/// Route a request and dispatch to the appropriate handler.
pub(super) async fn handle_request(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    remote_addr: Option<std::net::IpAddr>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    // gRPC bodies are streaming — skip body collection and dispatch directly to tonic.
    // Middleware runs as a gate check on the headers, then forwards to tonic.
    #[cfg(feature = "grpc")]
    let hyper_req = match try_dispatch_grpc(hyper_req, dispatch, remote_addr, ctx.is_tls).await {
        Ok(resp) => return resp,
        Err(req) => req,
    };

    // Pre-body classification: determine route type and check for internal routes.
    // The borrow of hyper_req is released before any mutable access or body collection.
    let (route_class, internal_route, pre_method) =
        match RequestHead::from_hyper_request(&hyper_req, remote_addr, ctx.is_tls) {
            Some(head) => {
                let rc = dispatch.classify_route(&head);
                let ir = match_internal_route_from_path(head.path(), ctx);
                #[cfg(feature = "profiling")]
                let ir = ir.or_else(|| match_profiling_route(head.path(), head.query(), ctx));
                let m = head.method();
                (rc, ir, Some(m))
            }
            None => (RouteClass::Buffered, None, None),
        };

    // Internal routes (/health, /metrics, /debug/pprof/cpu) bypass body collection.
    if let Some(route) = internal_route {
        return dispatch_internal_head_only(
            &hyper_req,
            route,
            dispatch,
            ctx,
            remote_addr,
            pre_method.unwrap_or(super::method::Method::Get),
        )
        .await;
    }

    #[cfg(feature = "ws")]
    let is_ws_upgrade = hyper_req
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    #[cfg(not(feature = "ws"))]
    let is_ws_upgrade = false;

    let skip_body_collection = matches!(route_class, RouteClass::HeadOnly)
        || (matches!(&route_class, RouteClass::StreamingProxy { .. }) && is_ws_upgrade);

    match route_class {
        RouteClass::StreamingProxy {
            backend,
            prefix,
            params,
        } if !is_ws_upgrade => {
            return dispatch_streaming_proxy(
                hyper_req,
                dispatch,
                ctx,
                remote_addr,
                &backend,
                &prefix,
                params,
            )
            .await;
        }
        RouteClass::StreamingProxyUnhealthy => {
            return Ok(to_hyper_full(Response::text_raw(
                503,
                "service unavailable",
            )));
        }
        _ => {} // HeadOnly, Buffered, or WS upgrade on streaming proxy — continue
    }

    // Build (req, ws_upgrade) — head-only routes skip body collection,
    // buffered routes collect the full body before dispatch.
    #[cfg(feature = "ws")]
    let build_result: Result<(Request, WsUpgrade), hyper::Response<HyperResponseBody>> =
        match skip_body_collection {
            true => build_head_only_request_ws(hyper_req, remote_addr, ctx.is_tls).map_err(|b| *b),
            false => {
                collect_request(hyper_req, ctx.max_request_body, remote_addr, ctx.is_tls).await
            }
        };
    #[cfg(feature = "ws")]
    let (req, ws_upgrade) = match build_result {
        Ok(pair) => pair,
        Err(resp) => return Ok(resp),
    };

    #[cfg(not(feature = "ws"))]
    let build_result: Result<Request, hyper::Response<HyperResponseBody>> =
        match skip_body_collection {
            true => build_head_only_request(hyper_req, remote_addr, ctx.is_tls).map_err(|b| *b),
            false => {
                collect_request(hyper_req, ctx.max_request_body, remote_addr, ctx.is_tls).await
            }
        };
    #[cfg(not(feature = "ws"))]
    let req = match build_result {
        Ok(r) => r,
        Err(resp) => return Ok(resp),
    };

    let start = std::time::Instant::now();

    // Dispatch through the trie. Internal routes are already handled above.
    let result = dispatch.dispatch(req);

    // Non-standard dispatch types (WS, SSE, Stream) need a middleware gate check.
    // Async already runs middleware inside dispatch. Build the gate synchronously
    // so &DispatchResult is not held across an await (DispatchResult is not Sync).
    let gate_check = match result.needs_middleware_gate() {
        true => dispatch.middleware_gate(result.request_ref()),
        false => None,
    };

    let gate_blocked = match gate_check {
        None => None,
        Some(GateCheck { reached, fut }) => gate_result(reached, fut.await),
    };

    if let Some(blocked) = gate_blocked {
        let req = result.request_ref();
        record_request(ctx, req.method(), req.path(), blocked.status(), start);
        return Ok(to_hyper_full(blocked));
    }

    // Validate WebSocket Origin before accepting any upgrade.
    #[cfg(feature = "ws")]
    if let Some(rejected) = result
        .is_websocket()
        .then(|| ws_proxy::check_ws_origin(result.request_ref()))
        .flatten()
    {
        let req = result.request_ref();
        record_request(ctx, req.method(), req.path(), rejected.status(), start);
        return Ok(to_hyper_full(rejected));
    }

    match result {
        DispatchResult::Async(fut, req) => {
            let resp = strip_body_if_head(req.is_head(), fut.await);
            record_request(ctx, req.method(), req.path(), resp.status(), start);
            Ok(to_hyper_full(resp))
        }
        DispatchResult::Stream(fut, req) => handle_stream_response(fut.await, req, ctx, start),
        DispatchResult::Sse(handler, req) => {
            record_request(ctx, req.method(), req.path(), 200, start);
            handle_sse(handler, req, ctx.sse_buffer_size)
        }
        #[cfg(feature = "ws")]
        DispatchResult::WebSocket(handler, req) => {
            record_request(ctx, req.method(), req.path(), 101, start);
            ws_proxy::handle_ws_upgrade(ws_upgrade, handler, req, ctx.ws_buffer_size)
        }
        #[cfg(feature = "ws")]
        DispatchResult::ProxyWebSocket(req, backend, prefix) => {
            record_request(ctx, req.method(), req.path(), 101, start);
            ws_proxy::handle_proxy_ws(ws_upgrade, req, backend, prefix)
        }
        DispatchResult::ProxyStream(req, backend, prefix) => {
            handle_proxy_stream_response(req, &backend, &prefix, ctx, start).await
        }
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
    remote_addr: Option<std::net::IpAddr>,
    method: super::method::Method,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let start = std::time::Instant::now();

    match dispatch.skip_middleware_for_internal() {
        true => {
            let is_head = matches!(method, super::method::Method::Head);
            let resp = invoke_internal_route(&route);
            let resp = strip_body_if_head(is_head, resp);
            record_request(
                ctx,
                method.as_str(),
                hyper_req.uri().path(),
                resp.status(),
                start,
            );
            Ok(to_hyper_full(resp))
        }
        false => {
            dispatch_internal_through_middleware(
                hyper_req,
                route,
                dispatch,
                ctx,
                remote_addr,
                start,
            )
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
    remote_addr: Option<std::net::IpAddr>,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let head = match RequestHead::from_hyper_request(hyper_req, remote_addr, ctx.is_tls) {
        Some(h) => h,
        None => return Ok(to_hyper_full(Response::text_raw(405, "method not allowed"))),
    };
    let req = head.to_request(None);
    let handler = build_internal_handler(route);
    match dispatch.dispatch_with_handler(&handler, req) {
        DispatchResult::Async(fut, req) => {
            let resp = strip_body_if_head(req.is_head(), fut.await);
            record_request(ctx, req.method(), req.path(), resp.status(), start);
            Ok(to_hyper_full(resp))
        }
        _ => Ok(to_hyper_full(Response::text_raw(
            500,
            "internal dispatch error",
        ))),
    }
}

fn handle_sse(
    handler: super::router::SseHandler,
    req: Request,
    buffer_size: usize,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(buffer_size);

    let _task = crate::task::spawn(move || {
        let mut writer = SseWriter::new(tx);
        if let Err(e) = handler(&req, &mut writer) {
            tracing::warn!(error = %e, "SSE handler returned error");
        }
    });

    let body = StreamBody { rx };
    let builder = hyper::Response::builder()
        .status(200)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache");
    Ok(streaming_response_or_empty(builder, body))
}

/// Create an empty StreamBody (drained channel). Used for HEAD responses
/// and fallback error paths.
fn empty_stream_body() -> StreamBody {
    StreamBody {
        rx: tokio::sync::mpsc::channel(1).1,
    }
}

fn streaming_response_or_empty(
    builder: hyper::http::response::Builder,
    body: StreamBody,
) -> hyper::Response<HyperResponseBody> {
    builder
        .body(HyperResponseBody::Streaming(body))
        .unwrap_or_else(|err| {
            tracing::error!("failed to build streaming response: {err}");
            hyper::Response::new(HyperResponseBody::Streaming(empty_stream_body()))
        })
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

fn handle_stream_response(
    stream_resp: super::stream::StreamResponse,
    req: Request,
    ctx: &ConnCtx,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let is_head = req.is_head();
    let parts = stream_resp.into_parts();
    record_request(ctx, req.method(), req.path(), parts.status, start);

    let body = match is_head {
        true => empty_stream_body(),
        false => StreamBody { rx: parts.rx },
    };
    let mut builder = hyper::Response::builder().status(parts.status);
    for (name, value) in &parts.headers {
        builder = builder.header(name.as_ref(), value.as_ref());
    }

    Ok(streaming_response_or_empty(builder, body))
}

/// Forward a streaming proxy request to the backend and return a streaming hyper response.
async fn handle_proxy_stream_response(
    req: Request,
    backend: &str,
    prefix: &str,
    ctx: &ConnCtx,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let proxy_req = super::async_proxy::ProxyRequest::from_request(&req);
    let is_head = req.is_head();

    let upstream =
        match super::async_proxy::forward_request_streaming(proxy_req, backend, prefix).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "streaming proxy upstream failed");
                record_request(ctx, req.method(), req.path(), 502, start);
                return Ok(to_hyper_full(Response::text_raw(
                    502,
                    "proxy upstream failed",
                )));
            }
        };

    record_request(ctx, req.method(), req.path(), upstream.status, start);
    let mut builder = hyper::Response::builder().status(upstream.status);
    for (name, value) in upstream.headers.iter() {
        builder = builder.header(name.as_ref(), value.as_ref());
    }
    let body = match is_head {
        true => empty_stream_body(),
        false => StreamBody { rx: upstream.rx },
    };
    Ok(streaming_response_or_empty(builder, body))
}

/// Dispatch a streaming proxy request without buffering the incoming body.
///
/// Runs the middleware gate on a lightweight request (empty body), then
/// forwards the original hyper body stream to upstream.
async fn dispatch_streaming_proxy(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    ctx: &ConnCtx,
    remote_addr: Option<std::net::IpAddr>,
    backend: &str,
    prefix: &str,
    params: super::request::Params,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let start = std::time::Instant::now();
    let method =
        super::method::Method::from_hyper(hyper_req.method()).unwrap_or(super::method::Method::Get);
    let method_str = method.as_str();
    let is_head = matches!(method, super::method::Method::Head);

    // Middleware gate check using a lightweight Request (empty body).
    let gate_blocked =
        run_head_gate(&hyper_req, dispatch, remote_addr, ctx.is_tls, Some(params)).await;

    if let Some(blocked) = gate_blocked {
        record_request(
            ctx,
            method_str,
            hyper_req.uri().path(),
            blocked.status(),
            start,
        );
        return Ok(to_hyper_full(blocked));
    }
    let scheme = match ctx.is_tls {
        true => "https",
        false => "http",
    };

    let (hyper_parts, body) = hyper_req.into_parts();
    let path: Box<str> = hyper_parts.uri.path().into();
    let proxy_parts = super::async_proxy::IncomingProxyParts {
        method,
        path_and_query: hyper_parts
            .uri
            .path_and_query()
            .map_or("/", |pq| pq.as_str())
            .into(),
        headers: hyper_parts.headers,
        remote_addr,
        scheme,
    };

    let upstream =
        match super::async_proxy::forward_incoming_streaming(proxy_parts, body, backend, prefix)
            .await
        {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(error = %e, "streaming proxy upstream failed");
                record_request(ctx, method_str, &path, 502, start);
                return Ok(to_hyper_full(Response::text_raw(
                    502,
                    "proxy upstream failed",
                )));
            }
        };

    record_request(ctx, method_str, &path, upstream.status, start);
    let mut builder = hyper::Response::builder().status(upstream.status);
    for (name, value) in upstream.headers.iter() {
        builder = builder.header(name.as_ref(), value.as_ref());
    }
    let response_body = match is_head {
        true => empty_stream_body(),
        false => StreamBody { rx: upstream.rx },
    };
    Ok(streaming_response_or_empty(builder, response_body))
}

fn record_request(
    ctx: &ConnCtx,
    method: &'static str,
    path: &str,
    status: u16,
    start: std::time::Instant,
) {
    let elapsed = start.elapsed();

    if ctx.tracing_enabled {
        tracing::info!(
            method,
            path,
            status,
            latency_ms = elapsed.as_millis(),
            "request completed"
        );
    }

    if ctx.metrics_handle.is_some() {
        let status_label = status_to_label(status);
        metrics::counter!(
            "http_requests_total",
            "method" => method,
            "status" => status_label,
        )
        .increment(1);
        metrics::histogram!(
            "http_request_duration_seconds",
            "method" => method,
            "status" => status_label,
        )
        .record(elapsed.as_secs_f64());
    }
}

/// Return a static string label for an HTTP status code.
///
/// Common codes get `&'static str` with zero allocation. Rare codes
/// are cached in a fixed-size table initialized on first use — no memory leak.
fn status_to_label(status: u16) -> &'static str {
    match status {
        200 => "200",
        201 => "201",
        204 => "204",
        301 => "301",
        302 => "302",
        304 => "304",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        405 => "405",
        413 => "413",
        500 => "500",
        502 => "502",
        503 => "503",
        100..600 => {
            static TABLE: std::sync::OnceLock<Box<[Box<str>]>> = std::sync::OnceLock::new();
            let table = TABLE.get_or_init(build_status_text_table);
            &table[(status - 100) as usize]
        }
        _ => "unknown",
    }
}

/// Try to dispatch a gRPC request. Returns `Ok(response)` if the request was
/// gRPC (handled or blocked by middleware). Returns `Err(request)` if not gRPC,
/// giving the hyper request back for normal HTTP dispatch.
#[cfg(feature = "grpc")]
async fn try_dispatch_grpc(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<
    Result<hyper::Response<HyperResponseBody>, std::convert::Infallible>,
    hyper::Request<hyper::body::Incoming>,
> {
    let is_grpc = dispatch.grpc_router().is_some() && is_grpc_request(&hyper_req);
    match is_grpc {
        false => Err(hyper_req),
        true => Ok(dispatch_grpc_inner(hyper_req, dispatch, remote_addr, is_tls).await),
    }
}

/// Run the middleware gate and dispatch to tonic. Called only when the request
/// is confirmed gRPC and a grpc_router exists.
#[cfg(feature = "grpc")]
async fn dispatch_grpc_inner(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let grpc_router = match dispatch.grpc_router() {
        Some(r) => r,
        None => {
            return Ok(to_hyper_full(Response::text_raw(
                500,
                "grpc router missing",
            )));
        }
    };
    // Build a lightweight Request from headers for the middleware gate check.
    // The streaming gRPC body is preserved for tonic.
    let blocked = run_head_gate(&hyper_req, dispatch, remote_addr, is_tls, None).await;
    match blocked {
        Some(resp) => Ok(to_hyper_full(resp)),
        None => grpc_router.dispatch(hyper_req).await,
    }
}

/// Run middleware as a gate check for a streaming request (gRPC, streaming proxy).
///
/// Borrows URI and HeaderMap from the hyper request via `RequestHead`.
/// Only clones into an owned `Request` when middleware actually exists.
/// Returns `Some(response)` if middleware blocked, `None` if passed.
async fn run_head_gate(
    hyper_req: &hyper::Request<hyper::body::Incoming>,
    dispatch: &ServerDispatch,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
    params: Option<super::request::Params>,
) -> Option<Response> {
    let head = RequestHead::from_hyper_request(hyper_req, remote_addr, is_tls)?;
    let GateCheck { reached, fut } = dispatch.middleware_gate_head(&head, params)?;
    gate_result(reached, fut.await)
}
