use crate::RuntimeError;

pub(crate) fn mq_error(e: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::MessageQueue(e.to_string().into())
}
