//! The closed integration vocabulary, listed once and bound to its enums.
//!
//! The focused error contract and the terminal event rows both walk every
//! kind, operation, failure, and retryability. Each list sits here once, in
//! declaration order, and a compile-time check binds it to a `match` with no
//! wildcard arm, so a new variant fails to compile until it is listed.

use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability};

/// Every integration kind the closed public enum admits, in declaration order.
pub const KINDS: [IntegrationKind; 3] = [
    IntegrationKind::Nats,
    IntegrationKind::Sqs,
    IntegrationKind::Dns01,
];

/// The position of one kind in [`KINDS`].
///
/// A `match` with no wildcard arm, so a fourth kind fails to compile here until
/// it is given a row, and the list above cannot silently cover one kind fewer
/// than the enum holds.
const fn kind_row(kind: IntegrationKind) -> usize {
    match kind {
        IntegrationKind::Nats => 0,
        IntegrationKind::Sqs => 1,
        IntegrationKind::Dns01 => 2,
    }
}

/// Every operation the closed public enum admits, in declaration order.
pub const OPERATIONS: [IntegrationOperation; 14] = [
    IntegrationOperation::Connect,
    IntegrationOperation::Ready,
    IntegrationOperation::Publish,
    IntegrationOperation::Subscribe,
    IntegrationOperation::Receive,
    IntegrationOperation::Delete,
    IntegrationOperation::Close,
    IntegrationOperation::ZoneLookup,
    IntegrationOperation::CreateTxt,
    IntegrationOperation::DeleteTxt,
    IntegrationOperation::Provision,
    IntegrationOperation::CacheRead,
    IntegrationOperation::CacheWrite,
    IntegrationOperation::Renew,
];

/// The position of one operation in [`OPERATIONS`], with no wildcard arm.
const fn operation_row(operation: IntegrationOperation) -> usize {
    match operation {
        IntegrationOperation::Connect => 0,
        IntegrationOperation::Ready => 1,
        IntegrationOperation::Publish => 2,
        IntegrationOperation::Subscribe => 3,
        IntegrationOperation::Receive => 4,
        IntegrationOperation::Delete => 5,
        IntegrationOperation::Close => 6,
        IntegrationOperation::ZoneLookup => 7,
        IntegrationOperation::CreateTxt => 8,
        IntegrationOperation::DeleteTxt => 9,
        IntegrationOperation::Provision => 10,
        IntegrationOperation::CacheRead => 11,
        IntegrationOperation::CacheWrite => 12,
        IntegrationOperation::Renew => 13,
    }
}

/// Every failure the closed public enum admits, in declaration order.
pub const FAILURES: [IntegrationFailure; 12] = [
    IntegrationFailure::InvalidConfig,
    IntegrationFailure::Unavailable,
    IntegrationFailure::PermissionDenied,
    IntegrationFailure::Rejected,
    IntegrationFailure::Busy,
    IntegrationFailure::LimitExceeded,
    IntegrationFailure::Timeout,
    IntegrationFailure::Cancelled,
    IntegrationFailure::Closed,
    IntegrationFailure::OutcomeUnknown,
    IntegrationFailure::CleanupIncomplete,
    IntegrationFailure::InvalidCertificate,
];

/// The position of one failure in [`FAILURES`], with no wildcard arm.
const fn failure_row(failure: IntegrationFailure) -> usize {
    match failure {
        IntegrationFailure::InvalidConfig => 0,
        IntegrationFailure::Unavailable => 1,
        IntegrationFailure::PermissionDenied => 2,
        IntegrationFailure::Rejected => 3,
        IntegrationFailure::Busy => 4,
        IntegrationFailure::LimitExceeded => 5,
        IntegrationFailure::Timeout => 6,
        IntegrationFailure::Cancelled => 7,
        IntegrationFailure::Closed => 8,
        IntegrationFailure::OutcomeUnknown => 9,
        IntegrationFailure::CleanupIncomplete => 10,
        IntegrationFailure::InvalidCertificate => 11,
    }
}

/// Every retryability the closed public enum admits, in declaration order.
pub const RETRYABILITIES: [Retryability; 3] = [
    Retryability::Never,
    Retryability::Safe,
    Retryability::OutcomeUnknown,
];

/// The position of one retryability in [`RETRYABILITIES`], with no wildcard arm.
const fn retryability_row(retryability: Retryability) -> usize {
    match retryability {
        Retryability::Never => 0,
        Retryability::Safe => 1,
        Retryability::OutcomeUnknown => 2,
    }
}

/// Fail compilation unless each entry of `$list` sits at the row `$row` names.
///
/// A macro, because a `const` context cannot call through a function pointer.
macro_rules! assert_declaration_order {
    ($list:expr, $row:ident) => {{
        let mut row = 0;
        while row < $list.len() {
            assert!($row($list[row]) == row);
            row += 1;
        }
    }};
}

/// Each list sits in declaration order, and so holds every variant once.
///
/// Compile-time, because every input is a constant: a list that repeated one
/// variant and omitted another would fail here rather than in a runtime row
/// that could only count what it was handed.
const _: () = {
    assert_declaration_order!(KINDS, kind_row);
    assert_declaration_order!(OPERATIONS, operation_row);
    assert_declaration_order!(FAILURES, failure_row);
    assert_declaration_order!(RETRYABILITIES, retryability_row);
};
