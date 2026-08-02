pub use crossbeam_channel;

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
