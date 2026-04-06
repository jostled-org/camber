mod error;
#[cfg(feature = "nats")]
pub mod nats;
#[cfg(feature = "sqs")]
pub mod sqs;

#[cfg(any(feature = "nats", feature = "sqs"))]
pub(crate) use crate::runtime::block_on;
pub(crate) use error::mq_error;
