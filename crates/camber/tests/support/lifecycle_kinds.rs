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
    LifecycleFailure, LifecycleFailureKind, LifecycleFailures, LifecycleParticipant,
    LifecyclePhase, ResourceFailureKind, ResourcePhase, RuntimeError,
};

/// Every entry a returned aggregate holds, rendered on one line.
///
/// The projection a row reads when its claim is about the whole account rather
/// than about one entry it can name.
pub fn aggregate_rendering(error: &RuntimeError) -> Box<str> {
    aggregate(error).to_string().into_boxed_str()
}

/// The aggregate a runtime teardown returned, or a failure naming what it
/// returned instead.
///
/// Every root that reads a teardown result enters here, so "the returned error
/// is a lifecycle aggregate" is asserted in one place rather than restated
/// beside each row's own claim.
pub fn aggregate(error: &RuntimeError) -> &LifecycleFailures {
    match error {
        RuntimeError::Lifecycle(failures) => failures,
        other => panic!("expected a lifecycle aggregate, got {other:?}"),
    }
}

/// Every entry of a returned aggregate, in the order it froze them.
pub fn aggregate_identities(error: &RuntimeError) -> Box<[String]> {
    aggregate(error).iter().map(entry_identity).collect()
}

/// Assert the returned aggregate reports a scope drain that found `outstanding`
/// children still running.
///
/// The count is the whole claim several rows make, and reading it out of the
/// closed vocabulary once is what keeps each of them stating the number rather
/// than the shape of the error that carries it.
pub fn assert_scope_drain(error: &RuntimeError, outstanding: usize, context: &str) {
    let reported =
        aggregate(error)
            .iter()
            .find_map(|failure| match (failure.participant(), failure.kind()) {
                (
                    LifecycleParticipant::RootScope,
                    LifecycleFailureKind::ScopeDrainTimeout { outstanding },
                ) => Some(*outstanding),
                _ => None,
            });
    assert_eq!(
        reported,
        Some(outstanding),
        "{context}: {:?}",
        aggregate_identities(error)
    );
}

/// Assert the returned aggregate reports a Camber-owned child that unwound
/// carrying `payload`.
///
/// Reported anywhere in the account: every entry is a direct failure the
/// runtime reports, so a row that looked at one chosen entry would be asserting
/// a rendering order rather than the fault it cares about.
pub fn assert_background_panic(error: &RuntimeError, payload: &str, context: &str) {
    let reported = aggregate(error).iter().find_map(background_panic_payload);
    assert_eq!(
        reported,
        Some(payload),
        "{context}: {:?}",
        aggregate_identities(error)
    );
}

/// Read a Camber-owned child panic payload from one aggregate entry.
fn background_panic_payload(failure: &LifecycleFailure) -> Option<&str> {
    match (failure.participant(), failure.kind()) {
        (LifecycleParticipant::BackgroundTask, LifecycleFailureKind::TaskPanicked(payload)) => {
            Some(payload)
        }
        _ => None,
    }
}

/// Every closed participant, matched without a wildcard.
///
/// A resource carries its own name, so its entry is the one value here derived
/// from a registration rather than fixed by the enum.
pub fn participant_name(participant: &LifecycleParticipant) -> String {
    match participant {
        LifecycleParticipant::RootScope => "root-scope".to_owned(),
        LifecycleParticipant::BackgroundTask => "background-task".to_owned(),
        LifecycleParticipant::Resource(name) => format!("resource:{name}"),
        LifecycleParticipant::Exporter => "exporter".to_owned(),
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
