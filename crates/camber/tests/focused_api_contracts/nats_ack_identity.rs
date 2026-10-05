//! The reply-identity arithmetic of acknowledged NATS publishing, from the
//! production source itself.
//!
//! Arithmetic and its refusal class only. Public component rows prove that a
//! publish registers its token before submission and that replies route by
//! it; no row here runs a connection or executes 2^64 operations.

#[path = "../../src/mq/nats/ack/identity.rs"]
mod identity;

use camber::{IntegrationFailure, Retryability};
use identity::{ReplyTokens, TokensExhausted};

/// Issue `count` tokens from `tokens`, in order.
fn issued(tokens: &mut ReplyTokens, count: usize) -> Vec<Result<u64, TokensExhausted>> {
    (0..count).map(|_| tokens.issue()).collect()
}

#[test]
fn ack_identity_never_wraps_or_reuses() {
    let mut fresh = ReplyTokens::starting_at(0);
    assert_eq!(issued(&mut fresh, 3), vec![Ok(0), Ok(1), Ok(2)]);

    let mut ordinary = ReplyTokens::starting_at(41);
    assert_eq!(issued(&mut ordinary, 2), vec![Ok(41), Ok(42)]);

    // The penultimate value is the last one issued: issuing it would need
    // the maximum as the next token, and the maximum has no successor.
    let mut penultimate = ReplyTokens::starting_at(u64::MAX - 1);
    assert_eq!(
        issued(&mut penultimate, 3),
        vec![Ok(u64::MAX - 1), Err(TokensExhausted), Err(TokensExhausted)],
    );

    let mut exhausted = ReplyTokens::starting_at(u64::MAX);
    assert_eq!(
        issued(&mut exhausted, 2),
        vec![Err(TokensExhausted), Err(TokensExhausted)],
    );

    assert_eq!(
        TokensExhausted.classification(),
        (IntegrationFailure::LimitExceeded, Retryability::Never),
    );
}
