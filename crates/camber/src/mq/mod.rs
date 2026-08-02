#[cfg(any(feature = "nats", feature = "sqs"))]
mod blocking;
mod error;
#[cfg(feature = "nats")]
pub mod nats;
#[cfg(feature = "sqs")]
pub mod sqs;

#[cfg(any(feature = "nats", feature = "sqs"))]
pub(crate) use blocking::block_on;
pub(crate) use error::mq_error;
