#[path = "support/deterministic.rs"]
pub mod deterministic;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/rejection.rs"]
pub mod rejection_support;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[path = "support/streaming_multipart.rs"]
pub mod streaming_multipart;

#[path = "component_http_routing/async_client.rs"]
mod async_client;
#[path = "component_http_routing/audit_http_rate_limit.rs"]
mod audit_http_rate_limit;
#[path = "component_http_routing/body_admission.rs"]
mod body_admission;
#[path = "component_http_routing/client_methods.rs"]
mod client_methods;
#[path = "component_http_routing/client_timeouts.rs"]
mod client_timeouts;
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
#[path = "component_http_routing/path_parameters.rs"]
mod path_parameters;
#[path = "component_http_routing/query_parameters.rs"]
mod query_parameters;
#[path = "component_http_routing/response_metadata.rs"]
mod response_metadata;
#[path = "component_http_routing/retry_policy.rs"]
mod retry_policy;
#[path = "component_http_routing/route_dispatch.rs"]
mod route_dispatch;
#[path = "component_http_routing/server_requests.rs"]
mod server_requests;
#[path = "component_http_routing/streaming_multipart_route.rs"]
mod streaming_multipart_route;
