pub mod common;
#[path = "support/deterministic.rs"]
pub mod deterministic;

#[cfg(feature = "ws")]
#[path = "component_websocket/backend_tls.rs"]
mod backend_tls;
#[cfg(feature = "ws")]
#[path = "component_websocket/callback_ownership.rs"]
mod callback_ownership;
#[path = "component_websocket/connection_limits.rs"]
mod connection_limits;
#[cfg(feature = "ws")]
#[path = "component_websocket/direction_endpoints.rs"]
mod direction_endpoints;
#[cfg(feature = "ws")]
#[path = "component_websocket/frame_properties.rs"]
mod frame_properties;
#[cfg(feature = "ws")]
#[path = "component_websocket/framework_rejections.rs"]
mod framework_rejections;
#[cfg(feature = "ws")]
#[path = "component_websocket/handshake.rs"]
mod handshake;
#[cfg(feature = "ws")]
#[path = "component_websocket/handshake_properties.rs"]
mod handshake_properties;
#[cfg(feature = "ws")]
#[path = "component_websocket/shared_binary_payloads.rs"]
mod shared_binary_payloads;
#[cfg(feature = "ws")]
#[path = "component_websocket/terminal_causality.rs"]
mod terminal_causality;
#[cfg(feature = "ws")]
#[path = "component_websocket/transport_ownership.rs"]
mod transport_ownership;
