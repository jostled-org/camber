//! Reply identities: checked, monotonic, and never reused on one connection.
//!
//! Self-contained: it names only public `camber` items, so a focused test
//! includes this file as written and proves the arithmetic production runs.

use camber::{IntegrationFailure, Retryability};

/// The tokens one connection issues, in order.
#[derive(Debug)]
pub(crate) struct ReplyTokens {
    next: u64,
}

/// Every token of the connection is spent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TokensExhausted;

impl ReplyTokens {
    /// Tokens that continue from `next`.
    pub(crate) const fn starting_at(next: u64) -> Self {
        Self { next }
    }

    /// Issue the next token.
    ///
    /// A token is issued only while it has a successor, so the counter never
    /// wraps and no token repeats.
    ///
    /// # Errors
    ///
    /// [`TokensExhausted`] once no successor remains, and on every call after.
    pub(crate) const fn issue(&mut self) -> Result<u64, TokensExhausted> {
        let token = self.next;
        match token.checked_add(1) {
            Some(next) => {
                self.next = next;
                Ok(token)
            }
            None => Err(TokensExhausted),
        }
    }
}

impl TokensExhausted {
    /// The refusal a publish reads before submission: the same connection
    /// can never issue another token.
    pub(crate) const fn classification(&self) -> (IntegrationFailure, Retryability) {
        (IntegrationFailure::LimitExceeded, Retryability::Never)
    }
}
