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

/// The exact upstream owner each proxy registration on one router froze.
///
/// Re-exported for the reason the accessor above is: a contract that read its
/// own copy of a policy would prove nothing about which client a registered
/// route actually forwards through, and sharing is only observable at the owner
/// itself.
pub use crate::http::router::frozen_proxy_client_identities;

/// The exact default and the exact validator every static-file entry point
/// freezes its ceiling through.
///
/// Re-exported for the reason the accessor above is: a focused contract that
/// read its own copy of the default, or its own zero check, would prove nothing
/// about the maximum a served file is actually read under.
pub use crate::http::static_files::{DEFAULT_STATIC_FILE_LIMIT, frozen_static_file_limit};

/// The exact default and the exact accessor a served profiling route freezes its
/// output maximum through.
///
/// Re-exported for the reason the static-file pair is: a focused contract that
/// read its own copy of the documented default would prove nothing about the
/// maximum a rendered profile is actually retained under.
#[cfg(feature = "profiling")]
pub use crate::http::server_policy::{
    DEFAULT_PROFILING_RESPONSE_LIMIT, frozen_profiling_response_limit,
};

/// The exact default every resource phase deadline starts at, and the exact
/// log one lifecycle freezes its aggregate through.
///
/// Re-exported for the reason the accessors above are: a contract that built
/// its own `LifecycleFailures` would prove its own ordering and its own
/// precedence, not the ones a teardown returns, and a contract that restated
/// the documented default would prove nothing about the deadline a callback
/// actually runs under.
pub use crate::lifecycle::{DEFAULT_RESOURCE_PHASE_DEADLINE, LifecycleFailureLog};

/// The exact floor no participant's deadline is narrowed below.
///
/// Re-exported for the reason the default above is: it is the value a spent
/// aggregate hands a callback, so a contract restating the figure would prove
/// its own hundred milliseconds rather than the grace a forced stop gives an
/// owner it has just told to end.
pub use crate::lifecycle::FORCED_JOIN_GRACE;

/// The exact resource budget a configured runtime froze.
///
/// Re-exported for the reason the limit accessors above are: a contract that
/// read its own copy of a budget would prove nothing about the value the
/// coordinator running a callback reads.
pub use crate::runtime::frozen_resource_budget;

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
