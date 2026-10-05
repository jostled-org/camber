//! The failures a local-service fixture reports.

use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The engine cannot be reached at all: an infrastructure failure, never
    /// a failed assertion.
    #[error("container engine {program} is unavailable: {detail}")]
    EngineUnavailable { program: Box<str>, detail: Box<str> },
    #[error("engine command `{command}` failed ({status}): {output}")]
    Engine {
        command: Box<str>,
        status: Box<str>,
        output: Box<str>,
    },
    #[error("invalid fixture input: {0}")]
    Input(Box<str>),
    #[error("fixture I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("{service} published no loopback endpoint: {output}")]
    Endpoint { service: Box<str>, output: Box<str> },
    #[error(
        "{service} at {endpoint} did not acknowledge readiness within {bound:?}: {detail}\n{logs}"
    )]
    Readiness {
        service: Box<str>,
        endpoint: SocketAddr,
        bound: Duration,
        detail: Box<str>,
        logs: Box<str>,
    },
    #[error("owned resources survived teardown: {0:?}")]
    Residue(Box<[Box<str>]>),
}
