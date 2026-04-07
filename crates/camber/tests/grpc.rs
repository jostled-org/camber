#![cfg(feature = "grpc")]

mod common;

use camber::http::{GrpcRouter, Response, Router};
use camber::runtime;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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
        let name = &request.into_inner().name;
        let reply = proto::HelloReply {
            message: format!("Hello, {name}!"),
        };
        Ok(tonic::Response::new(reply))
    }
}

#[test]
fn grpc_async_handler_responds() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(proto::HelloRequest {
                        name: "Async".into(),
                    }))
                    .await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, Async!");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_unary_call() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(proto::HelloRequest {
                        name: "Camber".into(),
                    }))
                    .await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, Camber!");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_reflection_lists_services() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);

            let reflection_service = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
                .build_v1()
                .unwrap();

            let grpc = GrpcRouter::new()
                .add_service(greeter_service)
                .add_service(reflection_service);

            let mut router = Router::new();
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let service_names: Vec<String> = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();

                let mut client =
                    tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient::new(
                        channel,
                    );

                let req = tonic_reflection::pb::v1::ServerReflectionRequest {
                    host: String::new(),
                    message_request: Some(
                        tonic_reflection::pb::v1::server_reflection_request::MessageRequest::ListServices(
                            String::new(),
                        ),
                    ),
                };

                let resp = client
                    .server_reflection_info(tokio_stream::once(req))
                    .await
                    .unwrap();

                let mut stream = resp.into_inner();
                use tokio_stream::StreamExt;
                let msg = stream.next().await.unwrap().unwrap();

                match msg.message_response {
                    Some(
                        tonic_reflection::pb::v1::server_reflection_response::MessageResponse::ListServicesResponse(
                            list,
                        ),
                    ) => list.service.into_iter().map(|s| s.name).collect(),
                    _ => Vec::new(),
                }
            });

            assert!(
                service_names.iter().any(|s| s == "greeter.Greeter"),
                "expected greeter.Greeter in services: {service_names:?}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_health_check() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);

            let (health_reporter, health_service) = tonic_health::server::health_reporter();
            common::block_on(async {
                health_reporter
                    .set_service_status("greeter.Greeter", tonic_health::ServingStatus::Serving)
                    .await;
            });

            let grpc = GrpcRouter::new()
                .add_service(greeter_service)
                .add_service(health_service);

            let mut router = Router::new();
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let status = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();

                let mut client = tonic_health::pb::health_client::HealthClient::new(channel);

                let resp = client
                    .check(tonic_health::pb::HealthCheckRequest {
                        service: "greeter.Greeter".into(),
                    })
                    .await
                    .unwrap();

                resp.into_inner().status
            });

            // 1 = SERVING
            assert_eq!(status, 1, "expected SERVING (1), got {status}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_blocks_unauthenticated_grpc() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_auth = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                match has_auth {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(401, "unauthorized").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            // gRPC request without auth header -> should be rejected
            let err = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(proto::HelloRequest {
                        name: "Camber".into(),
                    }))
                    .await
            });

            assert!(err.is_err(), "expected gRPC call to fail without auth");

            // gRPC request with auth header -> should succeed
            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                let mut req = tonic::Request::new(proto::HelloRequest {
                    name: "Camber".into(),
                });
                req.metadata_mut()
                    .insert("authorization", "Bearer token".parse().unwrap());
                client.say_hello(req).await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, Camber!");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_request_still_goes_through_header_guard_middleware() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_required = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("x-required-header"));
                match has_required {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(403, "missing required header").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            // gRPC request without the required header -> rejected
            let err = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(proto::HelloRequest {
                        name: "Blocked".into(),
                    }))
                    .await
            });

            assert!(
                err.is_err(),
                "expected gRPC call to fail without required header"
            );

            // gRPC request with the required header -> succeeds
            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                let mut req = tonic::Request::new(proto::HelloRequest {
                    name: "Allowed".into(),
                });
                req.metadata_mut()
                    .insert("x-required-header", "present".parse().unwrap());
                client.say_hello(req).await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, Allowed!");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_still_blocks_unauthenticated_grpc() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_auth = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                match has_auth {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(401, "unauthorized").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            // Unauthenticated gRPC request -> rejected before tonic
            let err = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(proto::HelloRequest {
                        name: "Camber".into(),
                    }))
                    .await
            });

            assert!(err.is_err(), "expected gRPC call to fail without auth");

            // Authenticated gRPC request -> reaches tonic successfully
            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                let mut req = tonic::Request::new(proto::HelloRequest {
                    name: "Camber".into(),
                });
                req.metadata_mut()
                    .insert("authorization", "Bearer token".parse().unwrap());
                client.say_hello(req).await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, Camber!");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_gate_path_still_handles_large_metadata_sets() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            let mw_counter = Arc::clone(&counter);

            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.use_middleware(move |req, next| {
                mw_counter.fetch_add(1, Ordering::SeqCst);
                next.call(req)
            });
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                let mut req = tonic::Request::new(proto::HelloRequest {
                    name: "MetadataTest".into(),
                });
                // Add many extra metadata headers
                for i in 0..50 {
                    let key: tonic::metadata::MetadataKey<tonic::metadata::Ascii> =
                        format!("x-extra-{i}").parse().unwrap();
                    req.metadata_mut()
                        .insert(key, format!("value-{i}").parse().unwrap());
                }
                client.say_hello(req).await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, MetadataTest!");

            let count = counter.load(Ordering::SeqCst);
            assert!(
                count >= 1,
                "expected middleware to run at least once, got {count}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn grpc_request_goes_through_logging_middleware() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(500))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let counter = Arc::new(AtomicUsize::new(0));
            let mw_counter = Arc::clone(&counter);

            let greeter_service = greeter_service::serve(MyGreeter);
            let grpc = GrpcRouter::new().add_service(greeter_service);

            let mut router = Router::new();
            router.use_middleware(move |req, next| {
                mw_counter.fetch_add(1, Ordering::SeqCst);
                next.call(req)
            });
            router.grpc(grpc);

            let addr = common::spawn_server(router);
            std::thread::sleep(Duration::from_millis(50));

            let response = common::block_on(async {
                let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
                    .unwrap()
                    .connect()
                    .await
                    .unwrap();
                let mut client = proto::greeter_client::GreeterClient::new(channel);
                client
                    .say_hello(tonic::Request::new(proto::HelloRequest {
                        name: "Camber".into(),
                    }))
                    .await
            });

            let reply = response.unwrap().into_inner();
            assert_eq!(reply.message, "Hello, Camber!");

            let count = counter.load(Ordering::SeqCst);
            assert!(
                count >= 1,
                "expected middleware to run at least once, got {count}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
