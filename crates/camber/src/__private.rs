pub use crossbeam_channel;

/// The exact pure operations the production body bound is built from.
///
/// Re-exported rather than reimplemented: a focused test that proved its own
/// copy of the arithmetic right would prove nothing about the copy a served
/// request runs. Neither reads a configured limit, invokes a policy, consumes a
/// body, or owns anything.
pub use crate::http::body_admission::{checked_body_frame_total, declared_length_exceeds_limit};

/// The exact accessor a registered buffered proxy route freezes its ceiling
/// through.
///
/// Re-exported for the same reason the two above are: a focused contract that
/// read a copy of the field would prove nothing about the value a route
/// actually froze.
pub use crate::http::proxy_policy::frozen_buffered_response_limit;

#[cfg(feature = "sqs")]
#[doc(hidden)]
pub fn sqs_message_id(message_id: Option<&str>) -> Result<Box<str>, crate::RuntimeError> {
    crate::mq::sqs::required_message_id(message_id)
}

#[cfg(feature = "sqs")]
#[doc(hidden)]
pub fn validate_sqs_receive(
    max_messages: i32,
    wait_time: std::time::Duration,
) -> Result<i32, crate::RuntimeError> {
    crate::mq::sqs::validate_receive_parameters(max_messages, wait_time)
}

#[cfg(feature = "nats")]
#[doc(hidden)]
pub fn closed_nats_try_next() -> Result<Option<crate::mq::nats::Message>, crate::RuntimeError> {
    crate::mq::nats::map_try_next_result(Ok(None))
}

#[cfg(feature = "dns01")]
#[doc(hidden)]
pub fn write_dns01_credentials(
    path: &std::path::Path,
    contents: &[u8],
) -> Result<(), crate::RuntimeError> {
    crate::dns01::write_credentials_file(path, contents)
}
