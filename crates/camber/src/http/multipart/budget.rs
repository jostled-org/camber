//! The single accounting owner for parser-created multipart allocations.
//!
//! Header storage, delimiter carry, metadata under construction, and pending
//! output all reserve the capacity they are about to retain before they grow,
//! and release it when that capacity transfers out or drops. A reservation above
//! the configured maximum fails as invalid multipart, so the refusal happens
//! while the bytes are still someone else's.
//!
//! One Hyper transport frame is deliberately outside this budget: the route does
//! not choose that allocation's size, so charging it here would be a number the
//! parser cannot honor.

use super::grammar::malformed;
use crate::RuntimeError;

/// A reservation the parser budget cannot honor.
const BUFFER_EXHAUSTED: &str = "multipart parser buffer exhausted";

/// The retained capacity one multipart session's parser has committed to hold.
///
/// Reservations are what this counts, not allocator behavior: a `Vec` that takes
/// more than it was asked for has slack, and slack is not retained data. What
/// the parser promised to hold is what a bound can be enforced against.
pub(super) struct MultipartBufferBudget {
    limit: usize,
    reserved: usize,
    peak: usize,
}

impl MultipartBufferBudget {
    /// Start a budget at one route's configured parser maximum.
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            reserved: 0,
            peak: 0,
        }
    }

    /// Reserve `bytes` more retained capacity, or refuse before it is taken.
    ///
    /// Overflow and a crossing are one answer because a caller must treat them
    /// the same: under either, the allocation this reservation was for never
    /// happens.
    pub(super) fn reserve(&mut self, bytes: usize) -> Result<(), RuntimeError> {
        let total = self
            .reserved
            .checked_add(bytes)
            .filter(|total| *total <= self.limit)
            .ok_or_else(|| malformed(BUFFER_EXHAUSTED))?;
        self.reserved = total;
        self.peak = self.peak.max(total);
        Ok(())
    }

    /// Release `bytes` of retained capacity that transferred out or dropped.
    ///
    /// Every release answers a reservation this budget already granted, so the
    /// clamp below is unreachable accounting rather than a policy: the assertion
    /// states that where the clamp is relied on.
    pub(super) fn release(&mut self, bytes: usize) {
        debug_assert!(
            bytes <= self.reserved,
            "a multipart release must answer capacity this budget reserved"
        );
        self.reserved = self.reserved.saturating_sub(bytes);
    }

    /// The capacity this budget is currently holding.
    pub(super) fn reserved(&self) -> usize {
        self.reserved
    }

    /// The most capacity this budget ever held.
    pub(super) fn peak(&self) -> usize {
        self.peak
    }
}
