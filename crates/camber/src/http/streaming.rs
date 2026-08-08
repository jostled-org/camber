use super::Request;
use super::body::{HyperResponseBody, StreamBody};
use super::handle::{ConnCtx, answer, answer_rejected, run_head_gate};
use super::record::record_scoped;
use super::rejection::{Rejected, RejectionScope, RequestIdentity};
use super::request::{RequestHead, RequestOrigin};
use super::response::HeaderPair;
use super::server_lifecycle::ConnectionLifecycle;
use super::sse::SseWriter;

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

/// Finish the SSE response, or answer with `fallback` if it cannot be built.
///
/// The status and both headers here are fixed text this function owns, so the
/// builder has nothing a peer supplied to reject. The fallback exists for the
/// shape of the API rather than for a failure anything can provoke, and it
/// keeps the status already recorded for this response: an SSE answer is a
/// `200` or it is nothing. Setting the status on a built response keeps the
/// fallback itself infallible — it cannot fail the way the response it stands
/// in for did.
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
/// HEAD requests get a drained body; all other methods stream from `rx`. The
/// scope answers whether this is a HEAD, because it is already what decides the
/// same question for a refusal in [`RejectionScope::convert`]. A stage that
/// re-derived the method would be a second place the rule could be decided, and
/// the copy that drifted would strip a body this one kept.
///
/// The failure travels rather than being swallowed. Nothing is on the wire yet,
/// so a status or header set that cannot be represented is a refusal like any
/// other pre-commitment failure — the buffered twin answers the identical fault
/// through the configured mapper. Answered here as a bare status, it counted
/// nothing, carried no `X-Request-Id`, and reached the operator only as a log
/// line naming no request.
fn build_streaming_response(
    status: u16,
    headers: &[HeaderPair],
    body: StreamBody,
    scope: &RejectionScope,
) -> Result<hyper::Response<HyperResponseBody>, hyper::http::Error> {
    let mut builder = hyper::Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(name.as_ref(), value.as_ref());
    }
    let body = match scope.is_head() {
        true => StreamBody::Drained,
        false => body,
    };
    builder.body(HyperResponseBody::Streaming(body))
}

/// Turn a streaming forward's outcome into the response it produces.
///
/// Both streaming-proxy entry points end here: a forward that failed before an
/// upstream response head is a refusal the route's own policy answers, and a
/// successful one carries the upstream's status and headers over the same
/// channel-backed body every streaming response uses. Stating it once keeps the
/// recorded status and the response shape from drifting between the two entry
/// points.
fn finish_upstream_stream(
    forwarded: Result<super::async_proxy::StreamingProxyResponse, super::async_proxy::ProxyFailure>,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    start: std::time::Instant,
) -> hyper::Response<HyperResponseBody> {
    let upstream = match forwarded {
        Ok(upstream) => upstream,
        Err(failure) => {
            return answer_rejected(ctx, scope, Rejected::from_proxy_failure(failure), start);
        }
    };
    // Recorded from the built response, not from the upstream's status: a
    // response that could not be built answers with its own status, and the
    // metric names what the peer was given.
    let response = match build_streaming_response(
        upstream.status,
        &upstream.headers,
        StreamBody::Proxy(upstream.rx),
        scope,
    ) {
        Ok(response) => response,
        Err(error) => {
            return answer_rejected(ctx, scope, Rejected::unrepresentable(error), start);
        }
    };
    record_scoped(ctx, scope, response.status().as_u16(), start);
    response
}

/// Answer a streaming handler's response, or refuse what cannot be represented.
///
/// The scope arrives from dispatch: a handler's status or header set that hyper
/// rejects is mapped by the same policy the handler's own failure would have
/// reached, and the answer is recorded under that same scope. One exit rule per
/// file — the refusal and the answer name the request the same way, so this
/// exit and its streaming-proxy sibling cannot disagree about what a request is
/// called.
///
/// The request is taken by value and released here rather than at the caller.
/// It carries the disconnect signal the producer behind `stream_resp` reports
/// through, so it outlives the response this builds.
pub(super) fn handle_stream_response(
    stream_resp: super::stream::StreamResponse,
    held_request: Request,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let parts = stream_resp.into_parts();
    let response = match build_streaming_response(
        parts.status,
        &parts.headers,
        StreamBody::Channel(parts.rx),
        scope,
    ) {
        Ok(response) => response,
        Err(error) => {
            return Ok(answer_rejected(
                ctx,
                scope,
                Rejected::unrepresentable(error),
                start,
            ));
        }
    };
    record_scoped(ctx, scope, response.status().as_u16(), start);
    drop(held_request);

    Ok(response)
}

/// Forward a streaming proxy request to the backend and return a streaming hyper response.
pub(super) async fn handle_proxy_stream_response(
    req: Request,
    backend: &str,
    prefix: &str,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let proxy_req = super::async_proxy::ProxyRequest::from_request(&req);

    let forwarded = super::async_proxy::forward_request_streaming(proxy_req, backend, prefix).await;
    Ok(finish_upstream_stream(forwarded, ctx, scope, start))
}

/// Dispatch a streaming proxy request without buffering the incoming body.
///
/// Runs the middleware gate on a lightweight request (empty body), then
/// forwards the original hyper body stream to upstream.
///
/// The method arrives already parsed, on the target the classification that
/// produced this route built. Only a matched route reaches here, so the method
/// is one Camber routes on, and parsing it again would only re-derive what the
/// head already proved.
///
/// `start` comes from `handle_request`, not from here. One clock per request
/// means every route class reports the same span into
/// `http_request_duration_seconds`; a second `Instant::now()` at this entry
/// would leave this class's buckets incomparable with all the others.
///
/// The child router arrives from classification rather than being resolved
/// again. Only a resolved child can produce this class, and the authority parse
/// and host-table search that selected it are exactly the two the gate below
/// otherwise repeated, on every proxied stream a `HostRouter` serves.
pub(super) async fn dispatch_streaming_proxy(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    ctx: &ConnCtx,
    target: super::dispatch::StreamingProxyTarget,
    origin: RequestOrigin<'_>,
    router: Option<&super::dispatch::FrozenRouter>,
    pre_body: &super::dispatch::PreBodyScope,
    start: std::time::Instant,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let super::dispatch::StreamingProxyTarget {
        backend,
        prefix,
        params,
        method,
    } = target;
    // The route and dispatch class classification established, carried into
    // every refusal this path can raise: the gate's, the host's, and the
    // upstream's alike.
    let scope = pre_body.scope(RequestIdentity::from_head(
        &origin,
        hyper_req.method(),
        hyper_req.uri(),
    ));

    // Middleware gate check using a lightweight Request (empty body). The gate
    // runs against the child classification already resolved, so an authority
    // Camber cannot parse never reaches here: that one is answered from the
    // head, before this class is chosen.
    //
    // A middleware response carrying a header name hyper cannot build leaves as
    // a `500`, and recording it before that conversion named a status the peer
    // never saw, so it goes out through the same answering path every other
    // refusal on this path takes.
    let head = RequestHead::from_hyper_request(&hyper_req, origin);
    if let Some(blocked) = run_head_gate(&head, router, Some(params), &scope).await {
        // Answered under the scope built above, not a rebuilt built-in one: a
        // gate response the wire cannot carry recovers through this route's own
        // mapper, the same one the host and upstream refusals reach.
        return Ok(answer(ctx, blocked, start, &scope));
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

    let forwarded =
        super::async_proxy::forward_incoming_streaming(proxy_parts, body, &backend, &prefix).await;
    Ok(finish_upstream_stream(forwarded, ctx, &scope, start))
}
