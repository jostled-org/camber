use super::encoding::decode_hex_pair;
use super::map_reqwest_error;
use super::method::Method;
use super::response::HeaderPair;
use crate::RuntimeError;
use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

static PROXY_CLIENT: LazyLock<Result<reqwest::Client, Arc<str>>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .map_err(|e| -> Arc<str> { e.to_string().into() })
});

pub(crate) fn proxy_client() -> Result<&'static reqwest::Client, RuntimeError> {
    PROXY_CLIENT
        .as_ref()
        .map_err(|e| RuntimeError::Http(Arc::clone(e)))
}

/// Check whether a header must not be forwarded between hops (RFC 2616 §13.5.1).
/// Uses pattern matching — zero-cost, no hashing overhead.
/// All comparisons are lowercase: hyper normalizes header names to lowercase.
fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

/// Check whether a header is a forwarded-metadata header that Camber sets itself.
/// Client-supplied values must be stripped before Camber adds its own to prevent
/// spoofing (e.g. a client injecting `X-Forwarded-For: 10.0.0.1`).
pub(super) fn is_forwarded_metadata(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-forwarded-for")
        || name.eq_ignore_ascii_case("x-forwarded-host")
        || name.eq_ignore_ascii_case("x-forwarded-proto")
        || name.eq_ignore_ascii_case("x-real-ip")
        || name.eq_ignore_ascii_case("forwarded")
}

pub(super) fn strip_prefix<'a>(path_and_query: &'a str, prefix: &str) -> Option<Cow<'a, str>> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let remainder = match path.strip_prefix(prefix) {
        Some("") => "/",
        Some(rest) => rest,
        None => path,
    };
    let has_traversal = remainder.split('/').any(is_dot_dot);
    match (has_traversal, query) {
        (true, _) => None,
        (false, Some(q)) => Some(Cow::Owned(format!("{remainder}?{q}"))),
        (false, None) => Some(Cow::Borrowed(remainder)),
    }
}

/// Check whether a path segment is `..` after percent-decoding.
///
/// Catches raw `..`, single-encoded `%2e%2e`, mixed `%2e.`, `.%2e`,
/// and double-encoded variants like `%252e%252e`.
fn is_dot_dot(segment: &str) -> bool {
    let decoded = percent_decode_segment(segment);
    decoded == ".."
}

/// Percent-decode a single path segment. Handles one level of encoding
/// then recurses once to catch double-encoding (`%252e` -> `%2e` -> `.`).
fn percent_decode_segment(input: &str) -> Cow<'_, str> {
    let first_pass = percent_decode_once(input);
    match matches!(first_pass, Cow::Borrowed(_)) {
        true => first_pass,
        false => Cow::Owned(percent_decode_once(first_pass.as_ref()).into_owned()),
    }
}

/// Single pass of percent-decoding over a string.
/// Returns `Cow::Borrowed` when no percent-encoding is present.
fn percent_decode_once(input: &str) -> Cow<'_, str> {
    match input.contains('%') {
        true => {}
        false => return Cow::Borrowed(input),
    }
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match bytes[i] {
            b'%' if i + 2 < bytes.len() => decode_hex_pair(bytes[i + 1], bytes[i + 2]),
            _ => None,
        };
        match decoded {
            Some(ch) => {
                result.push(ch as char);
                i += 3;
            }
            None => {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    Cow::Owned(result)
}

/// Owned data extracted from Request for async forwarding.
/// Owns its data to avoid holding a &Request borrow across .await points.
pub(super) struct ProxyRequest {
    pub(super) method: Method,
    pub(super) path: Box<str>,
    pub(super) headers: Box<[HeaderPair]>,
    pub(super) body: bytes::Bytes,
    pub(super) remote_addr: Option<std::net::IpAddr>,
    pub(super) scheme: &'static str,
}

impl ProxyRequest {
    pub(super) fn from_request(req: &super::Request) -> Self {
        Self {
            method: req.method_enum(),
            path: req.raw_path_and_query().into(),
            headers: req
                .headers()
                .map(|(k, v)| (Cow::Owned(k.to_owned()), Cow::Owned(v.to_owned())))
                .collect(),
            body: req.body_raw(),
            remote_addr: req.remote_addr(),
            scheme: match req.is_tls() {
                true => "https",
                false => "http",
            },
        }
    }
}

fn to_reqwest_method(method: Method) -> reqwest::Method {
    match method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Put => reqwest::Method::PUT,
        Method::Delete => reqwest::Method::DELETE,
        Method::Patch => reqwest::Method::PATCH,
        Method::Head => reqwest::Method::HEAD,
        Method::Options => reqwest::Method::OPTIONS,
    }
}

/// Filter request headers onto a reqwest builder, returning the original Host value if present.
fn filter_request_headers<'a>(
    mut builder: reqwest::RequestBuilder,
    headers: impl Iterator<Item = (&'a str, &'a str)>,
) -> (reqwest::RequestBuilder, Option<&'a str>) {
    let mut original_host = None;
    for (name, value) in headers {
        match (
            is_hop_by_hop(name),
            name.eq_ignore_ascii_case("host"),
            is_forwarded_metadata(name),
        ) {
            (true, true, _) => original_host = Some(value),
            (true, _, _) | (_, _, true) => {}
            _ => builder = builder.header(name, value),
        }
    }
    (builder, original_host)
}

/// Attach X-Forwarded-* headers and remote address to a reqwest builder.
fn attach_forwarding_metadata(
    mut builder: reqwest::RequestBuilder,
    original_host: Option<&str>,
    remote_addr: Option<std::net::IpAddr>,
    scheme: &str,
) -> reqwest::RequestBuilder {
    if let Some(host) = original_host {
        builder = builder.header("x-forwarded-host", host);
    }
    builder = builder.header("x-forwarded-proto", scheme);

    if let Some(addr) = remote_addr {
        let mut buf = [0u8; 45]; // max IPv6 text representation
        let addr_str = {
            use std::io::Write;
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            let _ = write!(cursor, "{addr}");
            let len = cursor.position() as usize;
            std::str::from_utf8(&buf[..len]).unwrap_or("")
        };
        builder = builder
            .header("x-forwarded-for", addr_str)
            .header("x-real-ip", addr_str);
    }

    builder
}

/// Create a reqwest builder with URL resolved from path and prefix.
///
/// Shared setup for both buffered and streaming upstream builders:
/// strip prefix, format URL, acquire client, create builder.
fn upstream_builder(
    method: Method,
    path_and_query: &str,
    backend: &str,
    prefix: &str,
) -> Result<reqwest::RequestBuilder, RuntimeError> {
    let remainder = match strip_prefix(path_and_query, prefix) {
        Some(r) => r,
        None => return Err(RuntimeError::InvalidArgument("invalid proxy path".into())),
    };
    let url = format!("{backend}{remainder}");
    let client = proxy_client()?;
    Ok(client.request(to_reqwest_method(method), &url))
}

/// Build a reqwest builder for upstream forwarding with a buffered body.
fn build_upstream_request(
    req: &ProxyRequest,
    backend: &str,
    prefix: &str,
) -> Result<reqwest::RequestBuilder, RuntimeError> {
    let builder = upstream_builder(req.method, &req.path, backend, prefix)?;

    let headers_iter = req.headers.iter().map(|(k, v)| (k.as_ref(), v.as_ref()));
    let (builder, original_host) = filter_request_headers(builder, headers_iter);
    let builder = attach_forwarding_metadata(builder, original_host, req.remote_addr, req.scheme);

    Ok(builder.body(req.body.clone()))
}

/// Metadata extracted from a hyper request for streaming proxy forwarding.
pub(super) struct IncomingProxyParts {
    pub(super) method: super::method::Method,
    pub(super) path_and_query: Box<str>,
    pub(super) headers: hyper::HeaderMap,
    pub(super) remote_addr: Option<std::net::IpAddr>,
    pub(super) scheme: &'static str,
}

/// Build a reqwest builder for upstream forwarding with a streaming incoming body.
fn build_upstream_request_streaming(
    parts: &IncomingProxyParts,
    incoming: hyper::body::Incoming,
    backend: &str,
    prefix: &str,
) -> Result<reqwest::RequestBuilder, RuntimeError> {
    let builder = upstream_builder(parts.method, &parts.path_and_query, backend, prefix)?;

    let headers_iter = parts
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), std::str::from_utf8(v.as_bytes()).unwrap_or("")));
    let (builder, original_host) = filter_request_headers(builder, headers_iter);
    let builder =
        attach_forwarding_metadata(builder, original_host, parts.remote_addr, parts.scheme);

    use futures_util::StreamExt;
    let body_stream = http_body_util::BodyStream::new(incoming).filter_map(|result| async move {
        match result {
            Ok(frame) => frame.into_data().ok().map(Ok),
            Err(e) => Some(Err(e)),
        }
    });

    Ok(builder.body(reqwest::Body::wrap_stream(body_stream)))
}

/// Collect non-hop-by-hop headers from an upstream response.
fn collect_response_headers(resp: &reqwest::Response) -> Box<[HeaderPair]> {
    let mut headers: Vec<HeaderPair> = Vec::with_capacity(resp.headers().len());
    for (name, value) in resp.headers() {
        match is_hop_by_hop(name.as_str()) {
            true => {}
            false => {
                let v = value.to_str().unwrap_or("");
                headers.push((
                    Cow::Owned(name.as_str().to_owned()),
                    Cow::Owned(v.to_owned()),
                ));
            }
        }
    }
    headers.into_boxed_slice()
}

/// Forward a request to upstream and return a buffered camber Response.
///
/// Proxy routes go through the middleware chain, so middleware can inspect
/// and modify the upstream response (status, headers). The body is fully
/// buffered into the Response.
pub(super) async fn forward_request_buffered(
    req: ProxyRequest,
    backend: &str,
    prefix: &str,
) -> Result<super::Response, RuntimeError> {
    let builder = build_upstream_request(&req, backend, prefix)?;
    let resp = builder.send().await.map_err(map_reqwest_error)?;
    let status = resp.status().as_u16();
    let headers = collect_response_headers(&resp);

    let body = resp.bytes().await.map_err(map_reqwest_error)?;
    let mut response = super::Response::bytes_raw(status, body);
    for (name, value) in headers.iter() {
        response = response.with_header(name, value);
    }
    Ok(response)
}

/// Forward a request to a backend service and return a buffered response.
///
/// Extracts owned data from the request (method, path, headers, body),
/// strips `prefix` from the path, forwards to `backend`, and returns
/// the upstream response with hop-by-hop headers removed.
///
/// Returns 502 on backend failure.
pub fn proxy_forward(
    req: &super::Request,
    backend: &str,
    prefix: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = super::Response> + Send>> {
    let proxy_req = ProxyRequest::from_request(req);
    let backend: Box<str> = backend.into();
    let prefix: Box<str> = prefix.into();
    Box::pin(async move {
        match forward_request_buffered(proxy_req, &backend, &prefix).await {
            Ok(resp) => resp,
            Err(_) => super::Response::text_raw(502, "bad gateway"),
        }
    })
}

/// Result of initiating a streaming proxy request.
/// Status and headers are buffered; the body streams via an mpsc channel.
pub(super) struct StreamingProxyResponse {
    pub(super) status: u16,
    pub(super) headers: Box<[HeaderPair]>,
    pub(super) rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
}

/// Spawn a task that streams response body chunks into an mpsc channel.
///
/// Shared between buffered-request and incoming-streaming proxy paths.
fn spawn_response_streamer(resp: reqwest::Response) -> tokio::sync::mpsc::Receiver<bytes::Bytes> {
    let (tx, rx) = tokio::sync::mpsc::channel(super::DEFAULT_CHANNEL_BUFFER);
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        while let Some(result) = stream.next().await {
            let bytes = match result {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(error = %e, "proxy upstream body read failed");
                    break;
                }
            };
            match tx.send(bytes).await {
                Ok(()) => {}
                Err(_) => break,
            }
        }
    });
    rx
}

/// Forward a request to upstream and stream the response body via a channel.
///
/// Status and headers are buffered; the body is forwarded chunk-by-chunk
/// with backpressure through the returned receiver.
pub(super) async fn forward_request_streaming(
    req: ProxyRequest,
    backend: &str,
    prefix: &str,
) -> Result<StreamingProxyResponse, RuntimeError> {
    let builder = build_upstream_request(&req, backend, prefix)?;
    let resp = builder.send().await.map_err(map_reqwest_error)?;
    let status = resp.status().as_u16();
    let headers = collect_response_headers(&resp);
    let rx = spawn_response_streamer(resp);

    Ok(StreamingProxyResponse {
        status,
        headers,
        rx,
    })
}

/// Forward an incoming hyper body stream to upstream without buffering.
///
/// The request body is streamed directly from the client to upstream,
/// bypassing the router's max_request_body limit. The response body
/// is streamed back via an mpsc channel.
pub(super) async fn forward_incoming_streaming(
    parts: IncomingProxyParts,
    incoming: hyper::body::Incoming,
    backend: &str,
    prefix: &str,
) -> Result<StreamingProxyResponse, RuntimeError> {
    let builder = build_upstream_request_streaming(&parts, incoming, backend, prefix)?;
    let resp = builder.send().await.map_err(map_reqwest_error)?;
    let status = resp.status().as_u16();
    let headers = collect_response_headers(&resp);
    let rx = spawn_response_streamer(resp);

    Ok(StreamingProxyResponse {
        status,
        headers,
        rx,
    })
}
