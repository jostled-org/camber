// Tests may panic; the workspace ban on panicking calls covers source only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]

#[path = "support/deterministic.rs"]
pub mod deterministic;
#[path = "support/error_chain.rs"]
pub mod error_chain;
#[path = "support/halt.rs"]
pub mod halt;
#[path = "support/http.rs"]
pub mod http;
#[path = "component_http_routing/turn_admission.rs"]
mod turn_admission;
// Mounted for the private child the connect-retry rows read their global
// capture in, and for the one the egress rows give a proxy environment.
#[path = "support/process.rs"]
pub mod process;
#[path = "support/rejection.rs"]
pub mod rejection_support;
#[path = "support/retry_upstream.rs"]
pub mod retry_upstream;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[path = "support/streaming_multipart.rs"]
pub mod streaming_multipart;
#[path = "support/temp.rs"]
pub mod temp;
// Mounted for the untrusted certificate the client-failure TLS peer presents.
#[path = "support/tls.rs"]
pub mod tls_support;
#[path = "support/trace_capture.rs"]
pub mod trace_capture;
// Mounted for the TLS peer the client-failure and retry rows refuse, and for
// the named one the egress rows authenticate.
#[path = "support/untrusted_tls.rs"]
pub mod untrusted_tls;
#[cfg(feature = "ws")]
#[path = "support/ws.rs"]
pub mod ws_support;

#[path = "component_http_routing/async_client.rs"]
mod async_client;
#[path = "component_http_routing/audit_http_rate_limit.rs"]
mod audit_http_rate_limit;
#[path = "component_http_routing/body_admission.rs"]
mod body_admission;
#[path = "component_http_routing/bounded_client.rs"]
mod bounded_client;
#[path = "component_http_routing/client_body.rs"]
mod client_body;
#[path = "component_http_routing/client_egress.rs"]
mod client_egress;
#[path = "component_http_routing/client_failures.rs"]
mod client_failures;
#[path = "component_http_routing/client_methods.rs"]
mod client_methods;
#[path = "component_http_routing/client_protocol_retry.rs"]
mod client_protocol_retry;
#[path = "component_http_routing/client_redirects.rs"]
mod client_redirects;
#[path = "component_http_routing/client_request.rs"]
mod client_request;
#[path = "component_http_routing/client_response.rs"]
mod client_response;
#[path = "component_http_routing/client_streaming.rs"]
mod client_streaming;
#[path = "component_http_routing/client_timeouts.rs"]
mod client_timeouts;
#[path = "component_http_routing/client_trust.rs"]
mod client_trust;
#[path = "component_http_routing/connection_persistence.rs"]
mod connection_persistence;
#[path = "component_http_routing/framework_rejections.rs"]
mod framework_rejections;
#[path = "component_http_routing/host_dispatch.rs"]
mod host_dispatch;
#[path = "component_http_routing/http_test_api.rs"]
mod http_test_api;
#[path = "component_http_routing/hyper_transport.rs"]
mod hyper_transport;
#[path = "component_http_routing/operation_commitment.rs"]
mod operation_commitment;
#[path = "component_http_routing/path_parameters.rs"]
mod path_parameters;
#[path = "component_http_routing/policy_inspection.rs"]
mod policy_inspection;
#[path = "component_http_routing/query_parameters.rs"]
mod query_parameters;
#[path = "component_http_routing/response_metadata.rs"]
mod response_metadata;
#[path = "component_http_routing/retry_lifetime.rs"]
mod retry_lifetime;
#[path = "component_http_routing/retry_policy.rs"]
mod retry_policy;
#[path = "component_http_routing/route_dispatch.rs"]
mod route_dispatch;
#[path = "component_http_routing/server_requests.rs"]
mod server_requests;
#[path = "component_http_routing/service_budgets.rs"]
mod service_budgets;
#[path = "component_http_routing/streaming_multipart_route.rs"]
mod streaming_multipart_route;
