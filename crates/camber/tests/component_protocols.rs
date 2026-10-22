// Tests may panic; the workspace ban on panicking calls covers source only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]

#[path = "support/h2_client.rs"]
pub mod h2_client;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/runtime.rs"]
pub mod runtime_support;

#[cfg(feature = "grpc")]
#[path = "component_protocols/grpc_services.rs"]
mod grpc_services;
#[path = "component_protocols/http2_negotiation.rs"]
mod http2_negotiation;
