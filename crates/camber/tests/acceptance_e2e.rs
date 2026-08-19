#[path = "support/h2_client.rs"]
pub mod h2_client;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/metrics_scrape.rs"]
pub mod metrics_scrape;
#[path = "support/process.rs"]
pub mod process;
// Mounted under a name of its own here, because this root's own multipart cases
// live in a module the inventory proof names `streaming_multipart`.
#[path = "support/streaming_multipart.rs"]
pub mod multipart_support;
#[path = "support/rejection_kinds.rs"]
pub mod rejection_kinds;
#[path = "support/rejection_metrics.rs"]
pub mod rejection_metrics;
#[path = "support/rejection.rs"]
pub mod rejection_support;
#[path = "support/resource_scripts.rs"]
pub mod resource_scripts;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[path = "support/service_operation.rs"]
pub mod service_operation;
// Mounted for the bounded thread join `service_operation` owns its load threads
// through, which is this suite's one spelling of that wait.
#[path = "support/stream.rs"]
pub mod stream;
#[path = "support/tls.rs"]
pub mod tls;
#[path = "support/trace_capture.rs"]
pub mod trace_capture;
#[cfg(feature = "ws")]
#[path = "support/ws.rs"]
pub mod ws;

pub mod common {
    pub use crate::h2_client::*;
    pub use crate::metrics_scrape::*;
    pub use crate::process::*;
    pub use crate::rejection_kinds::*;
    pub use crate::rejection_metrics::*;
    pub use crate::rejection_support::*;
    pub use crate::resource_scripts::*;
    pub use crate::runtime_support::*;
    pub use crate::service_operation::*;
    pub use crate::tls::*;
    pub use crate::trace_capture::*;
    #[cfg(feature = "ws")]
    pub use crate::ws::*;
}

#[path = "acceptance_e2e/async_server_and_scheduling.rs"]
pub mod async_server_and_scheduling;
#[path = "acceptance_e2e/body_admission.rs"]
pub mod body_admission;
#[path = "acceptance_e2e/concurrent_routes_and_keepalive.rs"]
pub mod concurrent_routes_and_keepalive;
#[path = "acceptance_e2e/cross_protocol_service_operation.rs"]
pub mod cross_protocol_service_operation;
#[path = "acceptance_e2e/framework_rejections.rs"]
pub mod framework_rejections;
#[path = "acceptance_e2e/host_routing_and_outbound_proxy.rs"]
pub mod host_routing_and_outbound_proxy;
#[path = "acceptance_e2e/mixed_content_and_websocket_proxy.rs"]
pub mod mixed_content_and_websocket_proxy;
#[path = "acceptance_e2e/routing_and_outbound_calls.rs"]
pub mod routing_and_outbound_calls;
#[path = "acceptance_e2e/service_deadlines.rs"]
pub mod service_deadlines;
#[path = "acceptance_e2e/service_operation_observability.rs"]
pub mod service_operation_observability;
#[path = "acceptance_e2e/streaming_multipart.rs"]
pub mod streaming_multipart;
#[path = "acceptance_e2e/unified_rest_sse_websocket.rs"]
pub mod unified_rest_sse_websocket;
