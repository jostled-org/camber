use crate::RuntimeError;
use std::future::Future;
use std::time::Duration;

/// Run a future with a timeout. Returns `Err(RuntimeError::Timeout)` on expiry.
pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, RuntimeError> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| RuntimeError::Timeout)
}

/// Clamp a duration to a minimum, logging a warning when the value is raised.
/// `name` identifies the setting in the log message.
pub(crate) fn clamp_duration(value: Duration, min: Duration, name: &str) -> Duration {
    match value < min {
        true => {
            tracing::warn!(requested = ?value, clamped = ?min, "{name} below minimum");
            min
        }
        false => value,
    }
}
