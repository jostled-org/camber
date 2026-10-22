// Tests may panic; the workspace ban on panicking calls covers source only.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]

pub mod common;
// The outbound client fixtures below reach `halt`, `http`, and `tls_support`
// through `super`. `common` has already compiled all three, so they are named
// here and not mounted a second time.
pub(crate) use common::{halt, http};
#[path = "support/error_chain.rs"]
pub mod error_chain;
#[path = "support/leaky_source.rs"]
pub mod leaky_source;
#[path = "support/retry_upstream.rs"]
pub mod retry_upstream;
pub mod tls_support {
    pub use crate::common::{build_server_config, generate_cert_with_san, is_alert_received};
}
#[path = "support/untrusted_tls.rs"]
pub mod untrusted_tls;

#[path = "component_observability/completion_facts.rs"]
mod completion_facts;
#[cfg(feature = "profiling")]
#[path = "component_observability/cpu_profiling.rs"]
mod cpu_profiling;
#[path = "component_observability/framework_rejections.rs"]
mod framework_rejections;
#[path = "component_observability/metrics_endpoint.rs"]
mod metrics_endpoint;
#[path = "component_observability/outbound_client.rs"]
mod outbound_client;
#[path = "component_observability/request_tracing.rs"]
mod request_tracing;
#[cfg(feature = "otel")]
#[path = "component_observability/trace_context.rs"]
mod trace_context;
