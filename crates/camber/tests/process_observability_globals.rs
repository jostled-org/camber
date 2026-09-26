pub mod common;

#[path = "process_observability_globals/install_once.rs"]
mod install_once;

#[cfg(feature = "otel")]
#[path = "process_observability_globals/otlp_export.rs"]
mod otlp_export;
