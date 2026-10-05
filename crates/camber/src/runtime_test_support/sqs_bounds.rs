//! The bounds an SQS builder carries into connect.

use crate::mq::sqs::SqsBuilder;
use std::time::Duration;

/// Reads the bounds a builder hands to connect, unchanged.
#[doc(hidden)]
pub struct SqsBoundsProbe;

/// One builder's bounds, as connect will apply them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqsBounds {
    pub connect_timeout: Duration,
    pub operation_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_in_flight: usize,
    pub max_message_bytes: usize,
}

impl SqsBoundsProbe {
    /// The bounds `builder` holds.
    #[must_use]
    pub const fn read(builder: &SqsBuilder) -> SqsBounds {
        let limits = builder.limits();
        SqsBounds {
            connect_timeout: limits.connect_timeout,
            operation_timeout: limits.operation_timeout,
            shutdown_timeout: limits.shutdown_timeout,
            max_in_flight: limits.max_in_flight,
            max_message_bytes: limits.max_message_bytes,
        }
    }
}
