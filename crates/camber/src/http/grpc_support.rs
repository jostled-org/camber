use super::grpc_handoff::GrpcRequestBody;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

/// The fixed identity a gRPC-dispatched request is named by.
///
/// The class is selected by content type against the `GrpcRouter` a router
/// registered, not by a path match in the trie, so it has no frozen pattern to
/// carry — the way an internal route has none. Every normalized trie pattern
/// begins with `/`, so this identity cannot read as one of them.
static GRPC_ROUTE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("grpc"));

/// The request tonic is handed: the peer's payload under the upload owner that
/// bounds it, not the raw incoming body.
type GrpcRequest = hyper::Request<GrpcRequestBody>;
type GrpcResponse = hyper::Response<tonic::body::Body>;
type GrpcFuture =
    Pin<Box<dyn Future<Output = Result<GrpcResponse, std::convert::Infallible>> + Send>>;

/// Type-erased gRPC service callable with a hyper request.
trait GrpcService: Send + Sync {
    fn call(&self, req: GrpcRequest) -> GrpcFuture;
}

/// Wraps a tonic-generated server type into a type-erased `GrpcService`.
struct TonicServiceWrapper<S> {
    service: S,
}

impl<S> GrpcService for TonicServiceWrapper<S>
where
    S: tonic::codegen::Service<
            GrpcRequest,
            Response = GrpcResponse,
            Error = std::convert::Infallible,
        > + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
{
    fn call(&self, req: GrpcRequest) -> GrpcFuture {
        let mut svc = self.service.clone();
        Box::pin(async move { tonic::codegen::Service::call(&mut svc, req).await })
    }
}

impl std::fmt::Debug for GrpcRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcRouter")
            .field("service_count", &self.services.len())
            .finish()
    }
}

/// Collects one or more tonic gRPC services for routing.
///
/// Requests with `content-type: application/grpc` are dispatched to
/// registered services by matching on the URI path prefix.
pub struct GrpcRouter {
    services: Vec<(Box<str>, Arc<dyn GrpcService>)>,
}

impl GrpcRouter {
    pub fn new() -> Self {
        Self {
            services: Vec::new(),
        }
    }

    /// Register a tonic-generated server (e.g. `GreeterServer<T>`).
    pub fn add_service<S>(mut self, service: S) -> Self
    where
        S: tonic::codegen::Service<
                GrpcRequest,
                Response = GrpcResponse,
                Error = std::convert::Infallible,
            > + tonic::server::NamedService
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
    {
        let prefix: Box<str> = format!("/{}/", S::NAME).into();
        self.services
            .push((prefix, Arc::new(TonicServiceWrapper { service })));
        self
    }

    /// Dispatch a gRPC request to the matching service.
    ///
    /// The answer leaves as tonic produced it. Committing it — and taking its
    /// body under this operation's download owner — belongs to the handoff
    /// coordinator, which is the one stage that knows whether a local cause
    /// beat this head.
    pub(super) async fn dispatch(&self, req: GrpcRequest) -> GrpcResponse {
        let path = req.uri().path();
        for (prefix, svc) in &self.services {
            if path.starts_with(prefix.as_ref()) {
                return answered(svc.call(req).await);
            }
        }

        grpc_unimplemented()
    }
}

impl Default for GrpcRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// A tonic service's error is `Infallible`, so an answer is its only outcome.
fn answered(result: Result<GrpcResponse, std::convert::Infallible>) -> GrpcResponse {
    result.unwrap_or_else(impossible_answer)
}

/// `Infallible` holds no value, so this arm is unreachable by construction.
fn impossible_answer(never: std::convert::Infallible) -> GrpcResponse {
    match never {}
}

/// The pattern the gRPC dispatch class establishes for every request it takes.
///
/// One shared identity, minted once: a refusal on this class costs a refcount
/// bump to name rather than an allocation on the request path.
pub(super) fn grpc_route() -> Arc<str> {
    Arc::clone(&GRPC_ROUTE)
}

/// Check if a request has gRPC content-type.
pub(super) fn is_grpc_request(req: &hyper::Request<hyper::body::Incoming>) -> bool {
    req.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/grpc"))
}

fn grpc_unimplemented() -> GrpcResponse {
    let mut resp = hyper::Response::new(tonic::body::Body::empty());
    *resp.status_mut() = hyper::StatusCode::OK;
    resp.headers_mut()
        .insert("grpc-status", hyper::header::HeaderValue::from_static("12"));
    resp.headers_mut().insert(
        "content-type",
        hyper::header::HeaderValue::from_static("application/grpc"),
    );
    resp
}
