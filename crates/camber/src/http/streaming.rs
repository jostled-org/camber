use super::body::{HyperResponseBody, StreamBody};
use super::handle::{ConnCtx, run_head_gate, to_hyper_full};
use super::record::record_request;
use super::server_lifecycle::ConnectionLifecycle;
use super::sse::SseWriter;
use super::{Request, Response};

pub(super) async fn handle_sse(
    handler: super::router::SseHandler,
    req: Request,
    buffer_size: usize,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    if let Some(script) = lifecycle.script() {
        script
            .pause(super::mock::LifecycleCheckpoint::SseBufferConfigured(
                buffer_size,
            ))
            .await;
    }
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

pub(super) fn handle_stream_response(
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
pub(super) async fn handle_proxy_stream_response(
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
pub(super) async fn dispatch_streaming_proxy(
    hyper_req: hyper::Request<hyper::body::Incoming>,
    dispatch: &super::router::ServerDispatch,
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
