//! The closed vocabulary a runtime's startup and teardown failures are stated
//! in, and the budget its resource callbacks run under.
//!
//! One owner for the facts a caller reads back off a runtime that could not
//! start or could not stop cleanly. Each concept sits in its own file — who
//! failed, in which phase, over what, and under which configured deadline — so
//! a new participant, phase, or failure kind is one deliberate edit rather than
//! an addition scattered across the coordinators that record them.
//!
//! The types are re-exported from the crate root; nothing here is reachable
//! through this module's own path.

mod failure;
mod failures;
mod participant;
mod phase;
mod resource_budget;
mod resource_failure;
mod resource_phase;
mod shutdown;
mod shutdown_owner;

pub(crate) use shutdown::AggregateShutdown;
pub(crate) use shutdown_owner::ShutdownOwner;

pub use shutdown::FORCED_JOIN_GRACE;

pub use failure::{LifecycleFailure, LifecycleFailureKind};
pub use failures::{LifecycleFailureLog, LifecycleFailures};
pub use participant::LifecycleParticipant;
pub use phase::LifecyclePhase;
pub use resource_budget::{DEFAULT_RESOURCE_PHASE_DEADLINE, ResourceBudget};
pub use resource_failure::{ResourceFailure, ResourceFailureKind};
pub use resource_phase::ResourcePhase;
