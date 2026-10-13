//! The streaming terminal of the outbound client.
//!
//! One task owns a streamed response from its first poll to the release of
//! its source. The caller holds the head and the one handle to the body.

mod operation;
mod response;
mod source;

pub(super) use operation::operate;
pub use response::ClientStreamResponse;
pub(in crate::http) use source::StreamMetrics;
