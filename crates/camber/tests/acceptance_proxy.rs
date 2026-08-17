#[path = "support/deterministic.rs"]
pub mod deterministic;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/raw_upstream.rs"]
pub mod raw_upstream;
#[path = "support/rejection.rs"]
pub mod rejection_support;
#[path = "support/runtime.rs"]
pub mod runtime_support;
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
#[path = "acceptance_proxy/streaming_forwarding.rs"]
mod streaming_forwarding;
#[path = "acceptance_proxy/transfer_budgets.rs"]
mod transfer_budgets;
#[path = "acceptance_proxy/websocket_forwarding.rs"]
mod websocket_forwarding;
