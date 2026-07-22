#[path = "support/http.rs"]
pub mod http;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[cfg(feature = "ws")]
#[path = "support/ws.rs"]
pub mod ws;

pub mod common {
    pub use crate::runtime_support::*;
    #[cfg(feature = "ws")]
    pub use crate::ws::*;
}

#[path = "acceptance_e2e/async_server_and_scheduling.rs"]
pub mod async_server_and_scheduling;
#[path = "acceptance_e2e/concurrent_routes_and_keepalive.rs"]
pub mod concurrent_routes_and_keepalive;
#[path = "acceptance_e2e/host_routing_and_outbound_proxy.rs"]
pub mod host_routing_and_outbound_proxy;
#[path = "acceptance_e2e/mixed_content_and_websocket_proxy.rs"]
pub mod mixed_content_and_websocket_proxy;
#[path = "acceptance_e2e/routing_and_outbound_calls.rs"]
pub mod routing_and_outbound_calls;
#[path = "acceptance_e2e/unified_rest_sse_websocket.rs"]
pub mod unified_rest_sse_websocket;
