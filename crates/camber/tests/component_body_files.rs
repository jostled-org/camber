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

#[path = "component_body_files/binary_bodies.rs"]
mod binary_bodies;
#[path = "component_body_files/body_admission.rs"]
mod body_admission;
#[path = "component_body_files/body_limits.rs"]
mod body_limits;
#[path = "component_body_files/bounded_collections.rs"]
mod bounded_collections;
#[path = "component_body_files/cookie_handling.rs"]
mod cookie_handling;
#[path = "component_body_files/framework_rejections.rs"]
mod framework_rejections;
#[path = "component_body_files/json_bodies.rs"]
mod json_bodies;
#[path = "component_body_files/multipart_forms.rs"]
mod multipart_forms;
#[path = "component_body_files/static_file_serving.rs"]
mod static_file_serving;
#[path = "component_body_files/streaming_multipart_core.rs"]
mod streaming_multipart_core;
#[path = "component_body_files/streaming_multipart_route.rs"]
mod streaming_multipart_route;
#[path = "component_body_files/urlencoded_forms.rs"]
mod urlencoded_forms;
