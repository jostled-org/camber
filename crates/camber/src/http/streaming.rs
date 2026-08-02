use super::body::{HyperResponseBody, StreamBody};
use super::handle::{
    ConnCtx, MethodNotAllowed, method_not_allowed_response, refuse_head, run_head_gate,
    to_hyper_full,
};
use super::record::record_request;
use super::request::{RequestOrigin, method_is_head};
use super::response::HeaderPair;
use super::server_lifecycle::ConnectionLifecycle;
use super::sse::SseWriter;
use super::{Request, Response};

/// Produce the SSE response and start the blocking producer that feeds it.
///
/// The producer may observe this response's lifetime through
/// `req.on_disconnect()`, but it never establishes the completion point: the
/// response body owns that, so there is one completion owner per response.
///
/// A HEAD gets the drained body its streaming siblings give it, and no producer
/// at all: Hyper writes no body for a HEAD, so a producer started here would
/// run against a channel nothing will ever read.
///
/// Inside the shutdown window — a runtime is established but the root scope
/// has already closed admission — the producer spawn is refused. The 200 has
/// been produced by then, so the refusal drops the unrun producer, its channel
/// sender goes with it, and the body ends its own stream immediately: a
/// zero-event response that still resolves through the same completion owner.
pub(super) async fn handle_sse(
    handler: super::router::SseHandler,
    req: Request,
    buffer_size: usize,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    super::mock::LifecycleScript::pause_at(
        lifecycle.script().as_deref(),
        super::mock::LifecycleCheckpoint::SseBufferConfigured(buffer_size),
    )
    .await;
    let body = match req.is_head() {
        true => StreamBody::Drained,
        false => spawn_sse_producer(handler, req, buffer_size),
    };
    let builder = hyper::Response::builder()
        .status(200)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache");
    // An SSE response is a `200` or it is nothing: the status is this
    // function's own, not an upstream's, so a builder failure still answers
    // with the status already recorded for it — with no events in it.
    Ok(streaming_response_or_empty(
        builder,
        body,
        hyper::StatusCode::OK,
    ))
}

/// Start the blocking SSE producer and hand back the body it feeds.
fn spawn_sse_producer(
    handler: super::router::SseHandler,
    req: Request,
    buffer_size: usize,
) -> StreamBody {
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(buffer_size);

    // Named before the request moves into the producer, so a refusal can say
    // which response lost its event source. The URI is `Bytes`-backed, so
    // holding one costs a refcount bump — not a copy of the path on every SSE
    // response for a name only the refusal branch ever reads.
    let uri = req.uri_owned();
    crate::task::spawn_internal_blocking("sse", uri.path(), move || {
        let mut writer = SseWriter::new(tx);
        if let Err(e) = handler(&req, &mut writer) {
            tracing::warn!(error = %e, "SSE handler returned error");
        }
    });

    StreamBody::Channel(rx)
}

/// Finish a streaming response, or answer with `fallback` if it cannot be built.
///
/// A builder failure means the status or a header the caller described is not
/// representable, so the response it asked for does not exist. The fallback
/// carries the status the caller names for that case rather than the builder's
/// bodyless `200`: a proxied `404` that could not be built is not an `OK`.
/// Setting the status on a built response keeps the fallback itself
/// infallible — it cannot fail the way the response it stands in for did.
fn streaming_response_or_empty(
    builder: hyper::http::response::Builder,
    body: StreamBody,
    fallback: hyper::StatusCode,
) -> hyper::Response<HyperResponseBody> {
    builder
        .body(HyperResponseBody::Streaming(body))
        .unwrap_or_else(|err| {
            tracing::error!("failed to build streaming response: {err}");
            let mut response =
                hyper::Response::new(HyperResponseBody::Streaming(StreamBody::Drained));
            *response.status_mut() = fallback;
            response
        })
}

/// Build a streaming hyper response from a status, header set, and body channel.
///
/// HEAD requests get a drained body; all other methods stream from `rx`.
/// A response that cannot be built is a `502`: the status and headers here are
/// an upstream's or a handler's, and neither is answerable once unrepresentable.
fn build_streaming_response(
    status: u16,
    headers: &[HeaderPair],
    body: StreamBody,
    is_head: bool,
) -> hyper::Response<HyperResponseBody> {
    let mut builder = hyper::Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name.as_ref(), value.as_ref());
    }
    let body = match is_head {
        true => StreamBody::Drained,
        false => body,
    };
    streaming_response_or_empty(builder, body, hyper::StatusCode::BAD_GATEWAY)
}

/// Turn a streaming forward's outcome into the response it produces.
///
/// Both streaming-proxy entry points end here: a failed forward is a `502`
/// whose buffered body owns its own completion, and a successful one carries
/// the upstream's status and headers over the same channel-backed body every
/// streaming response uses. Stating it once keeps the recorded status and the
/// response shape from drifting between the two entry points.
fn finish_upstream_stream(
    forwarded: Result<super::async_proxy::StreamingProxyResponse, crate::RuntimeError>,
    ctx: &ConnCtx,
    method: &'static str,
    path: &str,
    is_head: bool,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let upstream = match forwarded {
        Ok(upstream) => upstream,
        Err(e) => {
            tracing::warn!(error = %e, "streaming proxy upstream failed");
            record_request(ctx, method, path, 502, start);
            return to_hyper_full(Response::text_raw(502, "proxy upstream failed"));
        }
    };
    // Recorded from the built response, not from the upstream's status: a
    // response that could not be built answers with its own status, and the
    // metric names what the peer was given.
    let response = build_streaming_response(
        upstream.status,
        &upstream.headers,
        StreamBody::Proxy(upstream.rx),
        is_head,
    );
    record_request(ctx, method, path, response.status().as_u16(), start);
    response
}

pub(super) fn handle_stream_response(
    stream_resp: super::stream::StreamResponse,
    req: Request,
    ctx: &ConnCtx,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let is_head = req.is_head();
    let parts = stream_resp.into_parts();
    let response = build_streaming_response(
        parts.status,
        &parts.headers,
        StreamBody::Channel(parts.rx),
        is_head,
    );
    record_request(
        ctx,
        req.method(),
        req.path(),
        response.status().as_u16(),
        start,
    );

    Ok(response)
}

/// Forward a streaming proxy request to the backend and return a streaming hyper response.
pub(super) async fn handle_proxy_stream_response(
    req: Request,
    backend: &str,
    prefix: &str,
    ctx: &ConnCtx,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let proxy_req = super::async_proxy::ProxyRequest::from_request(&req);
    let is_head = req.is_head();

    let forwarded = super::async_proxy::forward_request_streaming(proxy_req, backend, prefix).await;
    Ok(finish_upstream_stream(
        forwarded,
        ctx,
        req.method(),
        req.path(),
        is_head,
        start,
    ))
}

/// Dispatch a streaming proxy request without buffering the incoming body.
///
/// Runs the middleware gate on a lightweight request (empty body), then
/// forwards the original hyper body stream to upstream.
///
/// `method` arrives already parsed, from the classification that produced this
/// route. A method Camber cannot name never reaches a proxy route — it is
/// answered as `Unnameable` before classification runs — so parsing it a second
/// time here would only re-derive what the head already proved, and would add a
/// refusal arm no request can reach.
///
/// `start` comes from `handle_request`, not from here. One clock per request
/// means every route class reports the same span into
/// `http_request_duration_seconds`; a second `Instant::now()` at this entry
/// would leave this class's buckets incomparable with all the others.
pub(super) async fn dispatch_streaming_proxy(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &super::router::ServerDispatch,
    ctx: &ConnCtx,
    target: super::dispatch::StreamingProxyTarget,
    origin: RequestOrigin<'_>,
    method: super::method::Method,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let super::dispatch::StreamingProxyTarget {
        backend,
        prefix,
        params,
    } = target;
    let method_str = method.as_str();
    let is_head = method_is_head(method);

    // Middleware gate check using a lightweight Request (empty body). A method
    // no gate could be built for is refused here rather than forwarded: an
    // upstream reached with the chain never run is an unauthenticated request.
    // The refusal is recorded like every other one this path answers with, so
    // no arm of this function can shed a request without moving a counter.
    let gate_blocked = match run_head_gate(&hyper_req, dispatch, origin, Some(params)).await {
        Ok(blocked) => blocked,
        Err(MethodNotAllowed) => {
            return Ok(refuse_head(
                ctx,
                Some(method),
                hyper_req.uri().path(),
                method_not_allowed_response(),
                start,
            ));
        }
    };

    // Answered through the same refusal path as the arm above it. A middleware
    // response carrying a header name hyper cannot build leaves as a `500`, and
    // recording it before that conversion named a status the peer never saw.
    if let Some(blocked) = gate_blocked {
        return Ok(refuse_head(
            ctx,
            Some(method),
            hyper_req.uri().path(),
            blocked,
            start,
        ));
    }
    let scheme = match origin.is_tls {
        true => "https",
        false => "http",
    };

    let (hyper_parts, body) = hyper_req.into_parts();
    let proxy_parts = super::async_proxy::IncomingProxyParts {
        method,
        path_and_query: hyper_parts
            .uri
            .path_and_query()
            .map_or("/", |pq| pq.as_str())
            .into(),
        headers: hyper_parts.headers,
        remote_addr: origin.remote_addr,
        scheme,
    };

    // `hyper_parts.uri` survives the partial move of `headers`, so the request
    // path is still readable for the record without a copy of it.
    let forwarded =
        super::async_proxy::forward_incoming_streaming(proxy_parts, body, &backend, &prefix).await;
    Ok(finish_upstream_stream(
        forwarded,
        ctx,
        method_str,
        hyper_parts.uri.path(),
        is_head,
        start,
    ))
}
