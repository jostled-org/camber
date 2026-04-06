use super::body::{GrpcBody, HyperResponseBody};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type GrpcRequest = hyper::Request<hyper::body::Incoming>;
type GrpcResponse = hyper::Response<tonic::body::BoxBody>;
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
    pub(super) async fn dispatch(
        &self,
        req: GrpcRequest,
    ) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
        let path = req.uri().path();
        for (prefix, svc) in &self.services {
            if path.starts_with(prefix.as_ref()) {
                let resp = svc.call(req).await?;
                let (parts, body) = resp.into_parts();
                return Ok(hyper::Response::from_parts(
                    parts,
                    HyperResponseBody::Grpc(GrpcBody {
                        inner: body,
                        finished: false,
                    }),
                ));
            }
        }

        Ok(grpc_unimplemented())
    }
}

impl Default for GrpcRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if a request has gRPC content-type.
pub(super) fn is_grpc_request(req: &hyper::Request<hyper::body::Incoming>) -> bool {
    req.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/grpc"))
}

fn grpc_unimplemented() -> hyper::Response<HyperResponseBody> {
    let mut resp = hyper::Response::new(HyperResponseBody::Full(http_body_util::Full::new(
        bytes::Bytes::new(),
    )));
    *resp.status_mut() = hyper::StatusCode::OK;
    resp.headers_mut()
        .insert("grpc-status", hyper::header::HeaderValue::from_static("12"));
    resp.headers_mut().insert(
        "content-type",
        hyper::header::HeaderValue::from_static("application/grpc"),
    );
    resp
}
