#[path = "support/http.rs"]
pub mod http;
#[path = "support/runtime.rs"]
pub mod runtime_support;

#[path = "component_middleware/cross_origin.rs"]
mod cross_origin;
#[path = "component_middleware/middleware_execution.rs"]
mod middleware_execution;
#[path = "component_middleware/rate_limiting.rs"]
mod rate_limiting;
#[path = "component_middleware/request_validation.rs"]
mod request_validation;
#[path = "component_middleware/response_compression.rs"]
mod response_compression;
