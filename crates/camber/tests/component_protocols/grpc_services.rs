use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use camber::http::{GrpcRouter, Request, Response, Router};
use camber::runtime;
use futures_util::future::Either;

use crate::runtime_support;

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(5);

mod proto {
    tonic::include_proto!("greeter");

    pub const FILE_DESCRIPTOR_SET: &[u8] =
        tonic::include_file_descriptor_set!("greeter_descriptor");
}

use proto::greeter_service;

struct MyGreeter;

#[tonic::async_trait]
impl greeter_service::Greeter for MyGreeter {
    async fn say_hello(
        &self,
        request: tonic::Request<proto::HelloRequest>,
    ) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
        let request = request.into_inner();
        Ok(tonic::Response::new(proto::HelloReply {
            message: format!("Hello, {}!", request.name),
        }))
    }
}

fn grpc_runtime() -> runtime::RuntimeBuilder {
    runtime_support::test_runtime()
        .header_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
}

fn spawn_grpc(grpc: GrpcRouter) -> SocketAddr {
    let mut router = Router::new();
    router.grpc(grpc);
    runtime_support::spawn_server(router)
}

fn block_on_protocol<F: Future>(future: F) -> F::Output {
    runtime_support::block_on(async {
        tokio::time::timeout(PROTOCOL_TIMEOUT, future)
            .await
            .expect("gRPC protocol operation timed out")
    })
}

async fn channel(addr: SocketAddr) -> tonic::transport::Channel {
    tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap()
}

async fn say_hello(
    addr: SocketAddr,
    request: tonic::Request<proto::HelloRequest>,
) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
    let mut client = proto::greeter_client::GreeterClient::new(channel(addr).await);
    client.say_hello(request).await
}

fn hello_request(name: &str) -> tonic::Request<proto::HelloRequest> {
    tonic::Request::new(proto::HelloRequest { name: name.into() })
}

/// The decoding ceiling tonic owns for this fixture's service.
const TONIC_MESSAGE_CEILING: usize = 64;

#[test]
fn grpc_route_keeps_tonic_body_ownership_under_http_admission_policy() {
    grpc_runtime()
        .run(|| {
            let port = crate::http::reserve_observed();
            let asked = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new().max_request_body(0);
            router.grpc(GrpcRouter::new().add_service(
                greeter_service::serve(MyGreeter).max_decoding_message_size(TONIC_MESSAGE_CEILING),
            ));
            let router = router.body_admission(crate::http::refusing_body_admission(&asked));
            let server = port.serve(router);
            let addr = server.addr();

            let reply = block_on_protocol(say_hello(addr, hello_request("Tonic")))
                .expect("a normal unary call succeeds under a restrictive HTTP body policy")
                .into_inner();
            assert_eq!(reply.message, "Hello, Tonic!");

            let oversized = block_on_protocol(say_hello(
                addr,
                hello_request(&"n".repeat(TONIC_MESSAGE_CEILING * 8)),
            ))
            .expect_err("tonic refuses a message above its own decoding ceiling");
            assert_eq!(
                oversized.code(),
                tonic::Code::OutOfRange,
                "the ceiling that refused is tonic's: {oversized:?}"
            );

            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "a gRPC request reaches no Camber body policy"
            );
            assert_eq!(server.controller().body_frames_polled(), 0);
            assert_eq!(server.controller().body_peak_retained_bytes(), 0);
            assert_eq!(server.controller().body_permit_owners_dropped(), 0);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_async_handler_responds() {
    grpc_runtime()
        .run(|| {
            let addr = spawn_grpc(GrpcRouter::new().add_service(greeter_service::serve(MyGreeter)));
            let reply = block_on_protocol(say_hello(addr, hello_request("Async")))
                .unwrap()
                .into_inner();
            assert_eq!(reply.message, "Hello, Async!");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_unary_call() {
    grpc_runtime()
        .run(|| {
            let addr = spawn_grpc(GrpcRouter::new().add_service(greeter_service::serve(MyGreeter)));
            let reply = block_on_protocol(say_hello(addr, hello_request("Camber")))
                .unwrap()
                .into_inner();
            assert_eq!(reply.message, "Hello, Camber!");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_reflection_lists_services() {
    grpc_runtime()
        .run(|| {
            let reflection = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
                .build_v1()
                .unwrap();
            let grpc = GrpcRouter::new()
                .add_service(greeter_service::serve(MyGreeter))
                .add_service(reflection);
            let addr = spawn_grpc(grpc);

            let service_names = block_on_protocol(async {
                let mut client = tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(channel(addr).await);
                let request = tonic_reflection::pb::v1::ServerReflectionRequest {
                    host: String::new(),
                    message_request: Some(
                        tonic_reflection::pb::v1::server_reflection_request::MessageRequest::ListServices(String::new()),
                    ),
                };
                let response = client
                    .server_reflection_info(tokio_stream::once(request))
                    .await
                    .unwrap();
                use tokio_stream::StreamExt;
                let message = response.into_inner().next().await.unwrap().unwrap();
                match message.message_response {
                    Some(
                        tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ListServicesResponse(list),
                    ) => list.service.into_iter().map(|service| service.name).collect::<Vec<_>>(),
                    _ => Vec::new(),
                }
            });
            assert!(
                service_names.iter().any(|name| name == "greeter.Greeter"),
                "expected greeter.Greeter in services: {service_names:?}"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_health_check() {
    grpc_runtime()
        .run(|| {
            let (health_reporter, health_service) = tonic_health::server::health_reporter();
            block_on_protocol(
                health_reporter
                    .set_service_status("greeter.Greeter", tonic_health::ServingStatus::Serving),
            );
            let grpc = GrpcRouter::new()
                .add_service(greeter_service::serve(MyGreeter))
                .add_service(health_service);
            let addr = spawn_grpc(grpc);
            let status = block_on_protocol(async {
                let mut client =
                    tonic_health::pb::health_client::HealthClient::new(channel(addr).await);
                client
                    .check(tonic_health::pb::HealthCheckRequest {
                        service: "greeter.Greeter".into(),
                    })
                    .await
                    .unwrap()
                    .into_inner()
                    .status
            });
            assert_eq!(status, 1, "expected SERVING (1), got {status}");
            runtime::request_shutdown();
        })
        .unwrap();
}

fn assert_header_guard(
    header: &'static str,
    header_value: &'static str,
    denied_status: u16,
    denied_body: &'static str,
    request_name: &'static str,
) {
    let (grpc_status, expected_code) = match denied_status {
        401 => ("16", tonic::Code::Unauthenticated),
        403 => ("7", tonic::Code::PermissionDenied),
        status => panic!("unsupported HTTP denial status for header guard: {status}"),
    };
    grpc_runtime()
        .run(|| {
            let grpc = GrpcRouter::new().add_service(greeter_service::serve(MyGreeter));
            let mut router = Router::new();
            router.use_middleware(move |request: &Request, next| {
                let allowed = request
                    .headers()
                    .any(|(name, _)| name.eq_ignore_ascii_case(header));
                match allowed {
                    true => Either::Left(next.call(request)),
                    false => Either::Right(std::future::ready(
                        Response::text(denied_status, denied_body)
                            .unwrap()
                            .with_content_type("application/grpc")
                            .with_header("grpc-status", grpc_status),
                    )),
                }
            });
            router.grpc(grpc);
            let addr = runtime_support::spawn_server(router);

            let denied = block_on_protocol(say_hello(addr, hello_request(request_name)));
            assert_eq!(
                denied.as_ref().map(|_| ()).map_err(tonic::Status::code),
                Err(expected_code),
                "expected gRPC call without {header} to fail with {expected_code:?}"
            );

            let mut allowed = hello_request(request_name);
            allowed
                .metadata_mut()
                .insert(header, header_value.parse().unwrap());
            let reply = block_on_protocol(say_hello(addr, allowed))
                .unwrap()
                .into_inner();
            assert_eq!(reply.message, format!("Hello, {request_name}!"));
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_blocks_unauthenticated_grpc() {
    assert_header_guard(
        "authorization",
        "Bearer token",
        401,
        "unauthorized",
        "Camber",
    );
}

/// The header the guarded rows admit on.
const GUARD_HEADER: &str = "x-required-header";
const GUARD_VALUE: &str = "present";

/// The credential the short-circuit row admits on.
const AUTH_HEADER: &str = "authorization";
const AUTH_VALUE: &str = "Bearer token";

/// The metadata a passing guard states over the provisional head.
const GUARD_PROJECTED: &str = "x-camber-projected";
const GUARD_PROJECTED_VALUE: &str = "applied";

/// The representation a passing guard states over that same provisional head.
///
/// tonic owns `content-type` on the head it finally commits, so a merge that
/// displaced the protocol's own correction leaves a client that cannot decode
/// the answer at all — which is what makes stating it here a discriminator
/// rather than an assertion about a name nothing reads.
const GUARD_TYPE: &str = "text/x-projected";

/// The service every guarded row dispatches to, and whether it was reached.
struct CountingGreeter {
    entered: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl greeter_service::Greeter for CountingGreeter {
    async fn say_hello(
        &self,
        request: tonic::Request<proto::HelloRequest>,
    ) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(proto::HelloReply {
            message: format!("Hello, {}!", request.into_inner().name),
        }))
    }
}

/// Serve the counting service under `guard`, and hand back both.
fn serve_guarded<F, Fut>(guard: F) -> (SocketAddr, Arc<AtomicUsize>)
where
    F: Fn(&Request, camber::http::Next) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    let entered = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new();
    router.use_middleware(guard);
    router.grpc(
        GrpcRouter::new().add_service(greeter_service::serve(CountingGreeter {
            entered: Arc::clone(&entered),
        })),
    );
    (runtime_support::spawn_server(router), entered)
}

/// The refusal a guard answers with instead of reaching tonic.
fn guard_refusal(status: u16, grpc_status: &str, body: &str) -> Response {
    Response::text(status, body)
        .expect("a valid refusal status")
        .with_content_type("application/grpc")
        .with_header("grpc-status", grpc_status)
}

/// State the passing chain's own metadata over the head tonic has not
/// committed yet.
fn projected(entered: camber::http::ResponseFuture) -> camber::http::ResponseFuture {
    Box::pin(async move {
        entered
            .await
            .with_header(GUARD_PROJECTED, GUARD_PROJECTED_VALUE)
            .with_content_type(GUARD_TYPE)
    })
}

/// Answer straight away, in the shape every guarded branch returns.
fn answering(response: Response) -> camber::http::ResponseFuture {
    Box::pin(std::future::ready(response))
}

/// 14.T2 cross-protocol projection.
///
/// The guard runs over a head tonic has not committed yet. A request without
/// the header it requires is refused before tonic is handed anything, and one
/// carrying it reaches the service — with what the chain stated over that
/// provisional head carried onto the head tonic finally wrote, under tonic's
/// own representation rather than the chain's.
#[test]
fn grpc_request_still_goes_through_header_guard_middleware() {
    grpc_runtime()
        .run(|| {
            let (addr, entered) = serve_guarded(|request: &Request, next| {
                let allowed = request
                    .headers()
                    .any(|(name, _)| name.eq_ignore_ascii_case(GUARD_HEADER));
                match allowed {
                    true => projected(next.call(request)),
                    false => answering(guard_refusal(403, "7", "missing required header")),
                }
            });

            let denied = block_on_protocol(say_hello(addr, hello_request("Allowed")));
            assert_eq!(
                denied.as_ref().map(|_| ()).map_err(tonic::Status::code),
                Err(tonic::Code::PermissionDenied),
                "a call without {GUARD_HEADER} must be refused by the guard",
            );
            assert_eq!(
                entered.load(Ordering::SeqCst),
                0,
                "the service ran behind a guard that refused the call",
            );

            let mut allowed = hello_request("Allowed");
            allowed
                .metadata_mut()
                .insert(GUARD_HEADER, GUARD_VALUE.parse().unwrap());
            // Decoding this at all is the representation claim: a merge that
            // displaced tonic's own `content-type` answers something no gRPC
            // client accepts.
            let answered = block_on_protocol(say_hello(addr, allowed))
                .expect("the guarded call reached the service");
            assert_eq!(
                answered
                    .metadata()
                    .get(GUARD_PROJECTED)
                    .and_then(|value| value.to_str().ok()),
                Some(GUARD_PROJECTED_VALUE),
                "the metadata the passing chain stated never reached the head tonic committed",
            );
            assert_eq!(answered.into_inner().message, "Hello, Allowed!");
            assert_eq!(
                entered.load(Ordering::SeqCst),
                1,
                "the passing chain admitted the call exactly once",
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

/// 14.T2 auth short-circuit.
///
/// Two ways a chain can refuse a gRPC call, and neither hands tonic anything:
/// a frame that answers without ever calling `Next`, and one that reaches the
/// gate terminal and then replaces what it was given. The replacement is an
/// answer, not permission to begin the handoff, so the service stays unentered
/// for both.
#[test]
fn auth_middleware_still_blocks_unauthenticated_grpc() {
    grpc_runtime()
        .run(|| {
            assert_unauthenticated_call_never_reaches_the_service(Refusal::BeforeTerminal);
            assert_unauthenticated_call_never_reaches_the_service(Refusal::ReplacingTheTerminal);
            runtime::request_shutdown();
        })
        .unwrap();
}

/// Where in the chain the refusal is raised.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Refusal {
    /// The frame answers without calling `Next` at all.
    BeforeTerminal,
    /// The frame calls `Next`, then replaces what the gate terminal gave it.
    ReplacingTheTerminal,
}

/// Serve one refusing shape, and require the service behind it never to run.
fn assert_unauthenticated_call_never_reaches_the_service(refusal: Refusal) {
    let (addr, entered) = serve_guarded(move |request: &Request, next| {
        let credentialed = request
            .headers()
            .any(|(name, _)| name.eq_ignore_ascii_case(AUTH_HEADER));
        match (credentialed, refusal) {
            (true, _) => projected(next.call(request)),
            (false, Refusal::BeforeTerminal) => answering(guard_refusal(401, "16", "unauthorized")),
            (false, Refusal::ReplacingTheTerminal) => {
                let reached = next.call(request);
                Box::pin(async move {
                    let _gate = reached.await;
                    guard_refusal(401, "16", "unauthorized")
                })
            }
        }
    });

    let denied = block_on_protocol(say_hello(addr, hello_request("Camber")));
    assert_eq!(
        denied.as_ref().map(|_| ()).map_err(tonic::Status::code),
        Err(tonic::Code::Unauthenticated),
        "{refusal:?}: an uncredentialed call must be refused by the chain",
    );
    assert_eq!(
        entered.load(Ordering::SeqCst),
        0,
        "{refusal:?}: the service ran behind a chain that refused the call",
    );

    let mut credentialed = hello_request("Camber");
    credentialed
        .metadata_mut()
        .insert(AUTH_HEADER, AUTH_VALUE.parse().unwrap());
    let answered =
        block_on_protocol(say_hello(addr, credentialed)).expect("the credentialed call succeeded");
    assert_eq!(
        answered
            .metadata()
            .get(GUARD_PROJECTED)
            .and_then(|value| value.to_str().ok()),
        Some(GUARD_PROJECTED_VALUE),
        "{refusal:?}: the credentialed call's head lost the chain's own metadata",
    );
    assert_eq!(answered.into_inner().message, "Hello, Camber!");
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "{refusal:?}: the credentialed call reached the service exactly once",
    );
}

fn assert_counting_middleware(request: tonic::Request<proto::HelloRequest>) -> proto::HelloReply {
    let counter = Arc::new(AtomicUsize::new(0));
    let middleware_counter = Arc::clone(&counter);
    let reply = grpc_runtime()
        .run(|| {
            let grpc = GrpcRouter::new().add_service(greeter_service::serve(MyGreeter));
            let mut router = Router::new();
            router.use_middleware(move |request, next| {
                middleware_counter.fetch_add(1, Ordering::SeqCst);
                next.call(request)
            });
            router.grpc(grpc);
            let addr = runtime_support::spawn_server(router);
            let reply = block_on_protocol(say_hello(addr, request))
                .unwrap()
                .into_inner();
            runtime::request_shutdown();
            reply
        })
        .unwrap();
    let count = counter.load(Ordering::SeqCst);
    assert!(
        count >= 1,
        "expected middleware to run at least once, got {count}"
    );
    reply
}

#[test]
fn grpc_gate_path_still_handles_large_metadata_sets() {
    let mut request = hello_request("MetadataTest");
    (0..50).for_each(|index| {
        let key: tonic::metadata::MetadataKey<tonic::metadata::Ascii> =
            format!("x-extra-{index}").parse().unwrap();
        request
            .metadata_mut()
            .insert(key, format!("value-{index}").parse().unwrap());
    });
    let reply = assert_counting_middleware(request);
    assert_eq!(reply.message, "Hello, MetadataTest!");
}

#[test]
fn grpc_request_goes_through_logging_middleware() {
    let reply = assert_counting_middleware(hello_request("Camber"));
    assert_eq!(reply.message, "Hello, Camber!");
}
