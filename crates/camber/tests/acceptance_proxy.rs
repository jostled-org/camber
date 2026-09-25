#[path = "support/deterministic.rs"]
pub mod deterministic;
#[path = "support/halt.rs"]
pub mod halt;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/process.rs"]
pub mod process;
#[path = "support/raw_upstream.rs"]
pub mod raw_upstream;
#[path = "support/rejection.rs"]
pub mod rejection_support;
#[path = "support/retry_upstream.rs"]
pub mod retry_upstream;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[path = "support/source_failure.rs"]
pub mod source_failure;
#[path = "support/stream.rs"]
pub mod stream;
#[path = "support/tls.rs"]
pub mod tls;
#[path = "support/trace_capture.rs"]
pub mod trace_capture;
#[cfg(feature = "ws")]
#[path = "support/ws.rs"]
pub mod ws;
#[cfg(feature = "ws")]
#[path = "support/ws_async.rs"]
pub mod ws_async;
#[cfg(feature = "ws")]
#[path = "support/ws_backend_script.rs"]
pub mod ws_backend_script;

pub mod common {
    pub use crate::deterministic::*;
    pub use crate::http::*;
    pub use crate::raw_upstream::*;
    pub use crate::rejection_support::*;
    pub use crate::runtime_support::*;
    pub use crate::tls::*;
    pub use crate::trace_capture::*;
    #[cfg(feature = "ws")]
    pub use crate::ws::*;
    #[cfg(feature = "ws")]
    pub use crate::ws_async::*;
}

#[path = "acceptance_proxy/backend_lifetime.rs"]
mod backend_lifetime;
#[path = "acceptance_proxy/backend_negotiation.rs"]
mod backend_negotiation;
#[path = "acceptance_proxy/body_admission.rs"]
mod body_admission;
#[path = "acceptance_proxy/bounded_buffers.rs"]
mod bounded_buffers;
#[path = "acceptance_proxy/buffered_forwarding.rs"]
mod buffered_forwarding;
#[path = "acceptance_proxy/downstream_flow_control.rs"]
mod downstream_flow_control;
#[path = "acceptance_proxy/framework_rejections.rs"]
mod framework_rejections;
#[path = "acceptance_proxy/header_properties.rs"]
mod header_properties;
#[path = "acceptance_proxy/streaming_forwarding.rs"]
mod streaming_forwarding;
#[path = "acceptance_proxy/transfer_budgets.rs"]
mod transfer_budgets;
#[path = "acceptance_proxy/websocket_forwarding.rs"]
mod websocket_forwarding;
