use super::Request;
use super::async_proxy::UploadDisposition;
use super::body::{HyperResponseBody, StreamBody};
use super::completion::CompletionAccount;
use super::handle::{ConnCtx, answer_framework, answer_rejected, run_head_gate};
use super::head_projection::{Gated, HeadProjection};
use super::operation::{InboundFailure, OperationEnvelope, OperationStage};
use super::rejection::{Rejected, RejectionScope, RequestIdentity};
use super::request::{RequestHead, RequestOrigin};
use super::response::HeaderPair;
use super::response_commitment::ResponseOrigin;
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
    operation: &OperationEnvelope,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let script = lifecycle.script();
    super::mock::LifecycleScript::pause_at_connection(
        script.as_deref(),
        super::mock::ConnectionOwnerEdge::SseBufferConfigured(buffer_size),
    )
    .await;
    // Resolved before the producer exists, from the registration's own policy
    // under the route's: the body that carries these events is bounded by one
    // owner, and a HEAD that produces none still resolves it the same way.
    let owner = operation.download(handler.budget(), script);
    let body = match req.is_head() {
        true => StreamBody::Drained,
        false => spawn_sse_producer(handler, req, buffer_size, owner),
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
    owner: super::transfer::TransferOwner,
) -> StreamBody {
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(buffer_size);

    // Named before the request moves into the producer, so a refusal can say
    // which response lost its event source. The URI is `Bytes`-backed, so
    // holding one costs a refcount bump — not a copy of the path on every SSE
    // response for a name only the refusal branch ever reads.
    let uri = req.uri_owned();
    crate::task::spawn_internal_blocking("sse", uri.path(), move || {
        let mut writer = SseWriter::new(tx);
        if let Err(e) = handler.produce(&req, &mut writer) {
            tracing::warn!(error = %e, "SSE handler returned error");
        }
    });

    StreamBody::channel(owner, rx)
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
/// A forward that failed before an upstream response head is a refusal the
/// route's own policy answers, and a successful one carries the upstream's
/// status and headers over the same channel-backed body every streaming
/// response uses.
fn finish_upstream_stream(
    forwarded: Result<
        super::async_proxy::StreamingProxyResponse,
        super::async_proxy::UploadFailure,
    >,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    account: &CompletionAccount,
    download: super::transfer::TransferOwner,
    operation: &OperationEnvelope,
) -> hyper::Response<HyperResponseBody> {
    let upstream = match forwarded {
        Ok(upstream) => upstream,
        Err(failure) => {
            return super::handle::answer_inbound_failure(
                ctx,
                scope,
                refused_forward(failure),
                account,
                operation,
            );
        }
    };
    // Recorded from the built response, not from the upstream's status: a
    // response that could not be built answers with its own status, and the
    // metric names what the peer was given.
    let response = match build_streaming_response(
        upstream.status,
        &disposed(upstream.headers, upstream.upload, scope),
        StreamBody::upstream(download, upstream.rx),
        scope,
    ) {
        Ok(response) => response,
        Err(error) => {
            return answer_rejected(ctx, scope, Rejected::unrepresentable(error), account);
        }
    };
    account.commit(scope, response.status().as_u16());
    response
}

/// Read one uncommitted streaming forward as the inbound disposition it
/// declares.
///
/// A conversion and nothing more. What the two dispositions owe the peer is
/// [`answer_inbound_failure`](super::handle::answer_inbound_failure)'s to say,
/// shared with the buffered reader and the multipart session: a second spelling
/// of those two arms is what let this exit map a refusal without naming the
/// framework on the commitment, and answer a committed cause as though the
/// payload had ended normally.
///
/// The upload's own refusal travels through unchanged: it was classified where
/// it was raised, under the limit or the transport that raised it, and
/// reclassifying it here as a proxy fault would tell the peer its upstream
/// failed when it was this request that was refused.
fn refused_forward(failure: super::async_proxy::UploadFailure) -> InboundFailure {
    match failure {
        super::async_proxy::UploadFailure::Proxy(failure) => {
            InboundFailure::Mapped(Rejected::from_proxy_failure(failure))
        }
        super::async_proxy::UploadFailure::Body(rejected) => InboundFailure::Mapped(rejected),
        // A response another producer already committed. There is nothing left
        // to map — a second status for one request is what the commitment makes
        // unreachable — so this forward only ends its transport, under the cause
        // that took the answer rather than under a payload end it never saw.
        super::async_proxy::UploadFailure::Committed(terminal) => InboundFailure::silent(terminal),
    }
}

/// State what this transport may do after an answer taken over unread payload.
///
/// An upstream head that arrived first stops the upload behind it, which leaves
/// framed payload no stage will read. On HTTP/1 the next request would be
/// framed out of those bytes, so the connection ends with this answer; the
/// scope owns that version question, and every refusal on this path already
/// answers it through the same rule.
///
/// Every disposition is listed rather than wildcarded, so a third one is a
/// compile error here: a state this function has not been taught about would
/// otherwise leave the transport open by default, which is the answer that
/// frames the next request out of payload nothing read.
fn disposed(
    headers: Box<[HeaderPair]>,
    upload: UploadDisposition,
    scope: &RejectionScope,
) -> Box<[HeaderPair]> {
    match (upload, scope.closes_transport()) {
        (UploadDisposition::Stopped, true) => headers
            .into_iter()
            .chain([super::rejection::close_header()])
            .collect(),
        (UploadDisposition::Stopped, false) | (UploadDisposition::Drained, _) => headers,
    }
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
///
/// The commitment is taken off the BUILT response, not off the handler's intent.
/// A status or header set the wire cannot carry is answered by the framework
/// mapper, and an origin recorded before the build named the application as the
/// producer of a head the application never got to give.
pub(super) fn handle_stream_response(
    stream_resp: super::stream::StreamResponse,
    held_request: Request,
    ctx: &ConnCtx,
    scope: &RejectionScope,
    account: &CompletionAccount,
    download: super::transfer::TransferOwner,
    operation: &OperationEnvelope,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let parts = stream_resp.into_parts();
    let response = match build_streaming_response(
        parts.status,
        &parts.headers,
        StreamBody::channel(download, parts.rx),
        scope,
    ) {
        Ok(response) => response,
        Err(error) => {
            return Ok(answer_framework(
                ctx,
                scope,
                Rejected::unrepresentable(error),
                account,
                operation,
            ));
        }
    };
    operation.commit_response(ResponseOrigin::Application);
    account.commit(scope, response.status().as_u16());
    drop(held_request);

    Ok(response)
}

/// The outbound request one streaming forward is built from, and its payload.
///
/// Taken apart here so the forward is handed exactly two things: what to send,
/// and the incoming stream to send as its body.
fn outbound_parts(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    method: super::method::Method,
    origin: &RequestOrigin<'_>,
) -> (
    super::async_proxy::IncomingProxyParts,
    hyper::body::Incoming,
) {
    let scheme = match origin.is_tls {
        true => "https",
        false => "http",
    };
    let (hyper_parts, body) = hyper_req.into_parts();
    let parts = super::async_proxy::IncomingProxyParts {
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
    (parts, body)
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
/// The account arrives from the served request, not from here. One clock per
/// request means every route class reports the same span into
/// `http_request_duration_seconds`; a second `Instant::now()` at this entry
/// would leave this class's buckets incomparable with all the others.
///
/// The child router arrives from classification rather than being resolved
/// again. Only a resolved child can produce this class, and the authority parse
/// and host-table search that selected it are exactly the two the gate below
/// otherwise repeated, on every proxied stream a `HostRouter` serves.
pub(super) async fn dispatch_streaming_proxy(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    target: super::dispatch::StreamingProxyTarget,
    router: Option<&super::dispatch::FrozenRouter>,
    pre_body: &super::dispatch::PreBodyScope,
    request_dispatch: &super::handle::RequestDispatch<'_>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let &super::handle::RequestDispatch { origin, .. } = request_dispatch;
    let super::dispatch::StreamingProxyTarget {
        backend,
        prefix,
        upstream,
        params,
        method,
        plan,
    } = target;
    // The route and dispatch class classification established, carried into
    // every refusal this path can raise: the admission's, the gate's, the
    // host's, and the upstream's alike.
    let scope = pre_body.scope(RequestIdentity::from_head(
        &origin,
        hyper_req.method(),
        hyper_req.uri(),
    ));
    let head = RequestHead::from_hyper_request(&hyper_req, origin);
    let (admitted, projection) = match cleared_to_forward(
        &head,
        Clearing {
            plan: &plan,
            params,
            router,
            scope: &scope,
            request_dispatch,
        },
    )
    .await
    {
        Cleared::Forward(admitted, projection) => (admitted, projection),
        Cleared::Answered(answered) => return Ok(answered),
    };
    let (proxy_parts, body) = outbound_parts(hyper_req, method, &origin);
    let answered = forwarded_stream(
        proxy_parts,
        body,
        admitted,
        Forwarding {
            target: super::async_proxy::ProxyTarget {
                backend: &backend,
                prefix: &prefix,
                upstream: &upstream,
            },
            scope: &scope,
            request_dispatch,
        },
    )
    .await;
    // Merged onto the head the upstream committed, so what a passing chain
    // stated survives without displacing anything the upstream decided.
    Ok(projection.merged_into(answered))
}

/// What one streaming proxy must clear before its upstream leg begins.
///
/// Two outcomes, not a success and a failure: a refused payload and a chain that
/// answered are both requests already served, and neither leaves an upstream leg
/// half-started behind it.
enum Cleared {
    /// The payload is admitted, and this is what a passing chain stated.
    Forward(super::body_admission::AdmittedBody, HeadProjection),
    /// Admission or the chain answered instead.
    Answered(hyper::Response<HyperResponseBody>),
}

/// What one streaming proxy is cleared against, beside its request head.
///
/// Grouped because clearing is one decision over all of it: the admission plan
/// the route resolved, the parameters its chain is shown, the child router whose
/// chain that is, the policy every refusal here is answered under, and the
/// operation whose total both halves spend.
struct Clearing<'a> {
    plan: &'a super::body_admission::ResolvedBodyPlan,
    params: super::request::Params,
    router: Option<&'a super::dispatch::FrozenRouter>,
    scope: &'a RejectionScope,
    request_dispatch: &'a super::handle::RequestDispatch<'a>,
}

/// Admit this request's payload and run its gate, or answer it here.
///
/// Admission runs before the gate, so a policy that declines the work never has
/// a middleware chain or an upstream leg run for it, and a gate that refuses
/// afterwards releases the permit admission granted.
///
/// The gate runs against the child classification already resolved, so an
/// authority Camber cannot parse never reaches here: that one is answered from
/// the head, before this class is chosen. Its answer is built under the scope
/// this route established rather than a rebuilt built-in one, so a gate response
/// the wire cannot carry recovers through this route's own mapper — the same one
/// the host and upstream refusals reach. Closing, because the payload this
/// request declared was never read: the next request on an HTTP/1 connection
/// would otherwise be framed out of it.
async fn cleared_to_forward(head: &RequestHead<'_>, clearing: Clearing<'_>) -> Cleared {
    let Clearing {
        plan,
        params,
        router,
        scope,
        request_dispatch,
    } = clearing;
    let &super::handle::RequestDispatch {
        ctx,
        lifecycle,
        account,
        operation,
        ..
    } = request_dispatch;
    let script = lifecycle.script();
    let admitted = match super::body_admission::admit(plan, head, script.as_ref()).await {
        Ok(admitted) => admitted,
        // The framework mapper produces this head, and the operation it produces
        // it for was minted before this class was chosen: a refusal answered
        // with no commitment reads back as a head no Camber owner reached.
        Err(rejected) => {
            return Cleared::Answered(answer_framework(ctx, scope, rejected, account, operation));
        }
    };
    operation.observe(script.as_deref(), OperationStage::Middleware);
    let gate = run_head_gate(head, router, Some(params), scope);
    let answering = super::handle::gate_within_total(gate, request_dispatch, scope, |blocked| {
        scope.closing(blocked)
    });
    match answering.await {
        Gated::Answered(answered) => Cleared::Answered(answered),
        Gated::Admitted(projection) => Cleared::Forward(admitted, projection),
    }
}

/// Where one streaming forward sends its payload, and under what.
///
/// Grouped because the forward is one decision over all of it: the upstream this
/// route froze, the prefix it rewrites, the policy every refusal on this path is
/// answered under, and the operation whose deadlines both legs spend.
struct Forwarding<'a> {
    target: super::async_proxy::ProxyTarget<'a>,
    scope: &'a RejectionScope,
    request_dispatch: &'a super::handle::RequestDispatch<'a>,
}

/// Forward one incoming stream and answer with what the upstream gave back.
///
/// The upload this route forwards is this operation's request payload, and
/// acquiring a usable upstream head is still pre-commit work, so both spend the
/// one request total the admitted head minted. The download is resolved after
/// that head commits, which is where its own lifetime starts.
async fn forwarded_stream(
    proxy_parts: super::async_proxy::IncomingProxyParts,
    body: hyper::body::Incoming,
    admitted: super::body_admission::AdmittedBody,
    forwarding: Forwarding<'_>,
) -> hyper::Response<HyperResponseBody> {
    let Forwarding {
        target,
        scope,
        request_dispatch,
    } = forwarding;
    let &super::handle::RequestDispatch {
        ctx,
        lifecycle,
        account,
        operation,
        ..
    } = request_dispatch;
    let script = lifecycle.script();
    operation.observe(script.as_deref(), OperationStage::Body);
    let forwarding = super::async_proxy::forward_incoming_streaming(
        proxy_parts,
        body,
        admitted,
        operation.upload(target.upstream.upload(), script.clone()),
        &target,
        operation.commitment(),
        script.as_ref(),
    );
    // Under the operation's own authority, not a bare deadline: a shutdown, a
    // forced cancellation, a peer whose lifetime ended, and an expired request
    // total are all local causes that must win over an upstream head this
    // service has not committed.
    let forwarded = match super::handle::pre_commit(forwarding, operation, script.as_deref()).await
    {
        Ok(forwarded) => forwarded,
        Err(failure) => {
            return super::handle::answer_inbound_failure(ctx, scope, failure, account, operation);
        }
    };
    operation.observe(script.as_deref(), OperationStage::ResponseHead);
    let download = operation.download(target.upstream.download(), script);
    finish_upstream_stream(forwarded, ctx, scope, account, download, operation)
}
