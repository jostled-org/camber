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
        .keepalive_timeout(Duration::from_millis(500))
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

#[test]
fn grpc_request_still_goes_through_header_guard_middleware() {
    assert_header_guard(
        "x-required-header",
        "present",
        403,
        "missing required header",
        "Allowed",
    );
}

#[test]
fn auth_middleware_still_blocks_unauthenticated_grpc() {
    assert_header_guard(
        "authorization",
        "Bearer token",
        401,
        "unauthorized",
        "Camber",
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
