//! The runtime's ownership of its managed integrations.
//!
//! One owner for every integration instance's admission, its operations'
//! reporting slots, and the failure history teardown hands to the lifecycle
//! aggregate. Each concept sits in its own file.

mod access;
mod accounts;
#[cfg(feature = "dns01")]
mod cleanup;
mod entry;
mod registry;
mod telemetry;

pub(crate) use access::IntegrationAccess;
#[cfg(any(feature = "nats", feature = "sqs"))]
pub(crate) use access::SubmissionMark;
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
pub(crate) use access::integration;
#[cfg(feature = "nats")]
pub(crate) use access::{CloseOwner, IntegrationMonitor};
pub(crate) use accounts::ReportAccounts;
#[cfg(any(feature = "nats", feature = "dns01"))]
pub(crate) use accounts::busy;
#[cfg(feature = "dns01")]
pub(crate) use cleanup::CleanupRegister;
pub(crate) use registry::IntegrationRegistry;
#[cfg(feature = "dns01")]
pub(crate) use telemetry::NestedTerminals;
#[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
pub(crate) use telemetry::TerminalOperation;

pub use access::OperationWaiter;
pub use accounts::{InstanceAccount, OperationAccount, PublishedFailure};
pub use entry::{IntegrationEntryObserver, IntegrationEntryState};
