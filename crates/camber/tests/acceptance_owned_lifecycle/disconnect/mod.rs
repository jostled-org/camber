//! Response-lifetime disconnect journeys, proven on real transport.
//!
//! Every case here enters through `req.on_disconnect()` on a served request:
//! the signal has no constructor outside a connection, so there is nothing
//! weaker to test against.

mod causes;
pub(crate) mod fixture;
#[cfg(feature = "grpc")]
mod grpc_completion;
mod http1_before_headers;
mod http2_stream_reset;
pub(crate) mod peer;
mod probes;
pub(crate) mod routes;
mod servers;
mod streaming_complete;
mod streaming_idle;
mod streaming_race;
mod streaming_spawned;
mod sync_path;
#[cfg(feature = "ws")]
mod websocket_handoff;
