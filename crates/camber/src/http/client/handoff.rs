//! The final head of one operation, and what the operation carries past it.
//!
//! The coordinator ends when it accepts a final head. What it began moves on
//! by value: the response, the lifetime fixed at the first poll, the attempt
//! timer the final send armed, and the transport the response arrived on. The
//! request does not move on, so nothing that holds a final head can send
//! again.

use super::lifetime::Lifetime;
use super::transport::OperationTransport;
use tokio::time::Instant;

/// One operation's final response, with the authority its body is read under.
///
/// The value is uniquely owned and holds no borrow of the terminal record.
/// The response is declared first, so it is released before the pool it
/// arrived on.
pub(super) struct FinalHead<'t> {
    /// The answer, with its body unread.
    pub(super) response: reqwest::Response,
    /// The operation's own lifetime. No handoff resets its deadline.
    pub(super) lifetime: Lifetime,
    /// When the final attempt expires, or `None` when it armed no timer. The
    /// instant is the one its send armed.
    pub(super) attempt: Option<Instant>,
    /// The transport the answer arrived on. A pinned one is released with
    /// this value, so its pool lives as long as the body it serves.
    pub(super) transport: OperationTransport<'t>,
    /// The most body bytes the request accepts, or `None` for no maximum.
    pub(super) limit: Option<usize>,
}
