#[path = "support/h2_client.rs"]
pub mod h2_client;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[path = "support/source_failure.rs"]
pub mod source_failure;
#[path = "support/stream.rs"]
pub mod stream_support;
#[path = "support/trace_capture.rs"]
pub mod trace_capture;

#[path = "component_streaming/server_sent_events.rs"]
mod server_sent_events;
#[path = "component_streaming/streamed_responses.rs"]
mod streamed_responses;
#[path = "component_streaming/transfer_budgets.rs"]
mod transfer_budgets;
#[path = "component_streaming/wire.rs"]
mod wire;
