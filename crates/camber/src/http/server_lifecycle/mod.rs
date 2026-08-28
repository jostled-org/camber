//! What one owned HTTP server is made of, split by who owns what.
//!
//! Three cohesive halves and nothing of its own. [`control`] holds the stop
//! vocabulary a public handle speaks and the waits every reader of it makes.
//! [`connections`] holds the owner tree: the connections a server registers,
//! and the requests and upgrades each connection contains. [`supervisor`] holds
//! the one task that admits sockets into that tree and settles it.
//!
//! This file re-exports and declares; it decides nothing. Keeping it empty is
//! what makes the split real — a fact that lived here would belong to no owner.

mod connections;
mod control;
mod supervisor;

pub(super) use connections::ConnectionLifecycle;
#[cfg(feature = "ws")]
pub(super) use connections::{ConnectionPermit, UpgradeRetention};
#[cfg(feature = "ws")]
pub(super) use connections::{
    UpgradeAdmission, UpgradeDispatchGate, UpgradeHandoff, UpgradeIdentity, UpgradeOwner,
    UpgradeRegistration, UpgradeTransportOwner,
};
pub(super) use control::{
    ConnectionShutdownDeadline, ServerControl, StopAuthority, wait_shutdown_control,
};
pub(super) use supervisor::{
    ServerContextSnapshot, ServerSupervisor, SupervisorJoin, poll_supervisor_join,
    supervisor_join_probe,
};
