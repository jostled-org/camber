pub mod common;

#[path = "component_websocket/connection_limits.rs"]
mod connection_limits;
#[cfg(feature = "ws")]
#[path = "component_websocket/direction_endpoints.rs"]
mod direction_endpoints;
#[cfg(feature = "ws")]
#[path = "component_websocket/framework_rejections.rs"]
mod framework_rejections;
#[cfg(feature = "ws")]
#[path = "component_websocket/handshake.rs"]
mod handshake;
#[cfg(feature = "ws")]
#[path = "component_websocket/transport_ownership.rs"]
mod transport_ownership;
