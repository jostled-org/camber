pub mod common;

// The runtime-scope builder, mounted from the root that owns it. One edit to
// the worker count it names has to reach the cases in both binaries, so the
// file is shared rather than restated here.
#[path = "component_runtime_concurrency/scope_builders.rs"]
pub mod scope_builders;

#[path = "component_runtime_resources/circuit_breaker.rs"]
mod circuit_breaker;
#[path = "component_runtime_resources/health_closing.rs"]
mod health_closing;
#[path = "component_runtime_resources/health_resources.rs"]
mod health_resources;
#[path = "component_runtime_resources/lifecycle_aggregate.rs"]
mod lifecycle_aggregate;
#[path = "component_runtime_resources/resource_lifecycle.rs"]
mod resource_lifecycle;
#[path = "component_runtime_resources/runtime_builder.rs"]
mod runtime_builder;
#[cfg(unix)]
#[path = "component_runtime_resources/runtime_signals.rs"]
mod runtime_signals;
#[path = "component_runtime_resources/shutdown_behavior.rs"]
mod shutdown_behavior;
#[path = "component_runtime_resources/worker_pool.rs"]
mod worker_pool;
