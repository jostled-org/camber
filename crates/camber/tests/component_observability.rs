pub mod common;

#[path = "component_observability/completion_facts.rs"]
mod completion_facts;
#[cfg(feature = "profiling")]
#[path = "component_observability/cpu_profiling.rs"]
mod cpu_profiling;
#[path = "component_observability/framework_rejections.rs"]
mod framework_rejections;
#[path = "component_observability/metrics_endpoint.rs"]
mod metrics_endpoint;
#[path = "component_observability/request_tracing.rs"]
mod request_tracing;
#[cfg(feature = "otel")]
#[path = "component_observability/trace_context.rs"]
mod trace_context;
