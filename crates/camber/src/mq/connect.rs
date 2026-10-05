//! The admission, connect bound, and connect terminal every message-queue
//! connection shares.

use crate::integration_lifecycle::{IntegrationAccess, TerminalOperation, integration};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
    RuntimeError,
};
use std::future::Future;
use std::time::Duration;

/// Admit a validated connection to the current runtime and establish it,
/// reporting one `Connect` terminal.
///
/// `settings` is the builder's whole validation. A refusal before admission,
/// that validation's included, is one terminal with no instance and no
/// duration, and performs no I/O. After admission the terminal is timed from
/// the runtime's admission and settles with what `establish` returns.
pub(super) async fn connect<S, C, F>(
    kind: IntegrationKind,
    settings: Result<S, IntegrationError>,
    max_in_flight: usize,
    establish: impl FnOnce(IntegrationAccess, S) -> F,
) -> Result<C, RuntimeError>
where
    F: Future<Output = Result<C, RuntimeError>>,
{
    let refusal = TerminalOperation::new(kind, IntegrationOperation::Connect, None);
    let (access, settings) = refusal.refusing(admit(kind, settings, max_in_flight))?;
    let connecting = access.connecting();
    connecting.settled(establish(access, settings).await)
}

/// Run an admitted instance's `establishing` work under the connect `bound`,
/// charging any failure to the instance.
///
/// A bound that passes first is a safe `Timeout`: connecting sends nothing to
/// the queue.
pub(super) async fn within_connect_bound<T>(
    access: &IntegrationAccess,
    kind: IntegrationKind,
    bound: Duration,
    establishing: impl Future<Output = Result<T, IntegrationError>>,
) -> Result<T, RuntimeError> {
    let established = match tokio::time::timeout(bound, establishing).await {
        Ok(established) => established,
        Err(_) => Err(IntegrationError::new(
            kind,
            IntegrationOperation::Connect,
            IntegrationFailure::Timeout,
            Retryability::Safe,
        )),
    };
    established.map_err(|error| integration(error.with_instance(access.id())))
}

/// Admit validated `settings` as one integration of the current runtime.
fn admit<S>(
    kind: IntegrationKind,
    settings: Result<S, IntegrationError>,
    max_in_flight: usize,
) -> Result<(IntegrationAccess, S), RuntimeError> {
    let settings = settings.map_err(integration)?;
    let access = crate::runtime::runtime_context()?.admit_integration(
        kind,
        IntegrationOperation::Connect,
        max_in_flight,
    )?;
    Ok((access, settings))
}
