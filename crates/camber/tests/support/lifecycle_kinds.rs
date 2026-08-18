//! The closed lifecycle and resource vocabularies, bound to the enums that
//! declare them.
//!
//! Two roots read this vocabulary and neither owns it. A focused root asserts
//! that every [`ResourcePhase`] spells its own operator-facing name, and a
//! component root reads a whole aggregate back through participant, stage, and
//! failure kind. Each root used to spell the phase matcher out for itself, so a
//! fourth phase would have forced one arm and left the other copy naming the
//! three it already knew, with both assertions still passing.
//!
//! What is enforced here is one claim: every closed value has a bounded name.
//! Each `match` has no `_` arm, so a new variant fails to compile until it is
//! given one, in the single place both roots read.
//!
//! The names are the test's own spelling, not a mirror of production's. The
//! production `label` methods are crate-private, and a matcher that called them
//! would agree with any renaming they did. A row that asserts a rendered line
//! reads production's `Display` and compares it against the name here, so the
//! two have to agree for the row to pass.
//!
//! Nothing in this module reaches another support module, so a root that needs
//! only the vocabulary mounts only this.

use camber::{
    LifecycleFailure, LifecycleFailureKind, LifecycleParticipant, LifecyclePhase,
    ResourceFailureKind, ResourcePhase,
};

/// Every closed participant, matched without a wildcard.
///
/// A resource carries its own name, so its entry is the one value here derived
/// from a registration rather than fixed by the enum.
pub fn participant_name(participant: &LifecycleParticipant) -> String {
    match participant {
        LifecycleParticipant::RootScope => "root-scope".to_owned(),
        LifecycleParticipant::Server => "server".to_owned(),
        LifecycleParticipant::Connection => "connection".to_owned(),
        LifecycleParticipant::Upgrade => "upgrade".to_owned(),
        LifecycleParticipant::BackgroundTask => "background-task".to_owned(),
        LifecycleParticipant::Resource(name) => format!("resource:{name}"),
        LifecycleParticipant::Exporter => "exporter".to_owned(),
        LifecycleParticipant::Executor => "executor".to_owned(),
    }
}

/// Every closed lifecycle stage, matched without a wildcard.
///
/// A resource callback names its own phase, so the stage a row reports stays
/// distinguishable from the four aggregate stages it never runs in.
pub fn phase_name(phase: LifecyclePhase) -> String {
    match phase {
        LifecyclePhase::Startup => "startup".to_owned(),
        LifecyclePhase::GracefulDrain => "graceful-drain".to_owned(),
        LifecyclePhase::ForcedJoin => "forced-join".to_owned(),
        LifecyclePhase::Resource(resource) => format!("resource:{}", resource_phase_name(resource)),
        LifecyclePhase::Finalize => "finalize".to_owned(),
    }
}

/// Every closed resource phase, matched without a wildcard.
pub fn resource_phase_name(phase: ResourcePhase) -> &'static str {
    match phase {
        ResourcePhase::StartupHealth => "startup-health",
        ResourcePhase::PeriodicHealth => "periodic-health",
        ResourcePhase::Shutdown => "shutdown",
    }
}

/// Every closed lifecycle failure kind, matched without a wildcard.
pub fn kind_name(kind: &LifecycleFailureKind) -> &'static str {
    match kind {
        LifecycleFailureKind::DeadlineExceeded(_) => "deadline",
        LifecycleFailureKind::Cancelled => "cancelled",
        LifecycleFailureKind::TaskPanicked(_) => "panicked",
        LifecycleFailureKind::ScopeDrainTimeout { .. } => "scope-drain",
        LifecycleFailureKind::Resource(_) => "resource",
        LifecycleFailureKind::JoinLost(_) => "join-lost",
        LifecycleFailureKind::Operation(_) => "operation",
    }
}

/// Every closed resource failure kind, matched without a wildcard.
pub fn resource_kind_name(kind: &ResourceFailureKind) -> &'static str {
    match kind {
        ResourceFailureKind::Returned(_) => "returned",
        ResourceFailureKind::Panicked(_) => "panicked",
        ResourceFailureKind::DeadlineExceeded => "deadline",
        ResourceFailureKind::LostWorker => "lost-worker",
        ResourceFailureKind::BlockedByActiveCallback => "blocked",
    }
}

/// How one entry reads for an order assertion: who, in which phase, over what.
pub fn entry_identity(failure: &LifecycleFailure) -> String {
    format!(
        "{}|{}|{}",
        participant_name(failure.participant()),
        phase_name(failure.phase()),
        kind_name(failure.kind())
    )
}
