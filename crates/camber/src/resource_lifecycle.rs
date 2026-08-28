//! The one coordinator that visits registered resources in order, bounds every
//! callback, publishes typed health, and hands every failure to the lifecycle
//! aggregate.
//!
//! Three phases enter here and none of them owns a second ordering, a second
//! bound, or a second way of naming what a callback did. The differences
//! between them are exactly three: which direction the registry is visited in,
//! which deadline the phase budget names, and whether the wait is an async one
//! inside the executor or a blocking one after it has drained.

mod worker;

use crate::lifecycle::{
    LifecycleFailureKind, LifecycleFailureLog, LifecycleParticipant, LifecyclePhase,
    ResourceFailureKind, ResourcePhase, ShutdownOwner,
};
use crate::resource::entry::ResourceEntry;
use crate::resource::registry::ResourceRegistry;
use crate::runtime_state::{LifecycleSignals, RuntimeInner};
use crate::runtime_test_support::ParticipantDisposition;
use std::ops::ControlFlow;
use std::sync::Arc;
use worker::{CallbackOutcome, WorkerContext};

/// Probe every registered resource once, before the user closure serves.
///
/// Every resource is visited even after one fails: a caller restarting a
/// service wants the whole account of what was unwell, not the first name in
/// registration order. Any failure at all means the closure never runs, and the
/// aggregate returned here is what `RuntimeBuilder::run` answers with.
pub(crate) async fn run_startup_health(
    runtime: &Arc<RuntimeInner>,
    registry: &ResourceRegistry,
) -> Option<crate::RuntimeError> {
    let context = WorkerContext::capture(runtime);
    let limit = runtime
        .config
        .resource_budget
        .phase(ResourcePhase::StartupHealth);
    let mut log = LifecycleFailureLog::new();
    for entry in registry.iter() {
        let outcome = worker::run_callback(
            entry,
            ResourcePhase::StartupHealth,
            limit,
            context.clone(),
            runtime,
        )
        .await;
        record(entry, ResourcePhase::StartupHealth, outcome, &mut log);
    }
    log.into_error()
}

/// Admit the one root-scope child that runs every later health pass.
///
/// One child for the whole registry, not one per resource: registration order
/// is a claim about when callbacks BEGIN, and parallel children could only
/// promise that they were all started.
pub(crate) fn admit_health_coordinator(runtime: &Arc<RuntimeInner>, registry: &ResourceRegistry) {
    if registry.is_empty() {
        return;
    }
    let interval = runtime.config.health_interval;
    let owned = Arc::clone(runtime);
    let entries = Arc::clone(registry);
    // The refusal is already reported against this subsystem's own name, and
    // the runtime setup that calls this has no result to propagate one through.
    drop(crate::task::admit_signalled_subsystem_on(
        runtime,
        "resource health",
        move |signals| run_health_coordinator(owned, entries, signals, interval),
    ));
}

/// Shut every eligible resource down, in reverse registration order.
///
/// Runs on the teardown thread after the root scope has drained, so no child
/// the executor can stop may still reach a resource. Every failure is retained
/// in the runtime's one teardown account: a resource that could not stop
/// cleanly is a caller-visible fact, not a log line.
///
/// Each callback waits no longer than the narrower of its own phase deadline
/// and what the aggregate shutdown has left, so a registry of slow resources
/// cannot spend one phase deadline apiece past the deadline the whole teardown
/// shares.
pub(crate) fn shutdown_resources(
    runtime: &RuntimeInner,
    registry: &ResourceRegistry,
    log: &mut LifecycleFailureLog,
) {
    let budget = runtime.config.resource_budget;
    let shutdown = runtime.shutdown_deadline();
    for entry in registry.iter().rev() {
        let owner = ShutdownOwner::resource(entry.name());
        let limit = budget.phase_deadline(ResourcePhase::Shutdown, shutdown.remaining(&owner));
        let outcome = worker::run_callback_blocking(entry, ResourcePhase::Shutdown, limit, runtime);
        let settled = match outcome.is_some() {
            true => ParticipantDisposition::Named,
            false => ParticipantDisposition::Completed,
        };
        record(entry, ResourcePhase::Shutdown, outcome, log);
        shutdown.settle(&owner, settled);
    }
}

/// Probe every resource on the configured interval until either lifecycle
/// signal fires.
async fn run_health_coordinator(
    runtime: Arc<RuntimeInner>,
    registry: ResourceRegistry,
    signals: LifecycleSignals,
    interval: std::time::Duration,
) {
    let context = WorkerContext::capture(&runtime);
    let limit = runtime
        .config
        .resource_budget
        .phase(ResourcePhase::PeriodicHealth);
    while let ControlFlow::Continue(()) = signals.tick(interval).await {
        let pass = visit_registry(&runtime, &registry, limit, context.as_ref(), &signals).await;
        if pass.is_break() {
            return;
        }
    }
}

/// Run one periodic pass over the whole registry, in registration order.
///
/// The wait for each callback breaks on the lifecycle signals as well as on the
/// phase deadline. Without that, a runtime asked to stop would still owe the
/// drain a full phase deadline per remaining resource — and the abandonment it
/// performs instead is the same one the deadline performs, so the resource
/// stays with its worker either way.
async fn visit_registry(
    runtime: &RuntimeInner,
    registry: &ResourceRegistry,
    limit: std::time::Duration,
    context: Option<&WorkerContext>,
    signals: &LifecycleSignals,
) -> ControlFlow<()> {
    for entry in registry.iter() {
        let callback = worker::run_callback(
            entry,
            ResourcePhase::PeriodicHealth,
            limit,
            context.cloned(),
            runtime,
        );
        match signals.guard(callback).await {
            ControlFlow::Break(()) => return ControlFlow::Break(()),
            ControlFlow::Continue(outcome) => {
                publish(entry, ResourcePhase::PeriodicHealth, outcome);
            }
        }
    }
    ControlFlow::Continue(())
}

/// Publish one callback's outcome and record it for the lifecycle aggregate.
///
/// The two phases that return an aggregate share this so neither can publish a
/// failure it then forgets to report, or report one it never published.
fn record(
    entry: &ResourceEntry,
    phase: ResourcePhase,
    outcome: CallbackOutcome,
    log: &mut LifecycleFailureLog,
) {
    if let Some(failure) = publish(entry, phase, outcome) {
        log.record(
            LifecycleParticipant::Resource(Arc::clone(entry.name())),
            LifecyclePhase::Resource(phase),
            LifecycleFailureKind::Resource(failure),
        );
    }
}

/// Publish one callback's outcome to this resource's health, reporting the
/// failure it named.
///
/// The operator event is emitted here, at the single writer, so a resource's
/// published state and the line an operator reads about it always describe the
/// same callback. Arbitrary cause text lives in the event's own field and never
/// in the closed name the endpoint renders.
fn publish(
    entry: &ResourceEntry,
    phase: ResourcePhase,
    outcome: CallbackOutcome,
) -> Option<crate::ResourceFailure> {
    if let Some(kind) = outcome.as_ref() {
        report(entry, phase, kind);
    }
    entry.publish(phase, outcome)
}

/// Report one failed callback to the operator.
fn report(entry: &ResourceEntry, phase: ResourcePhase, kind: &ResourceFailureKind) {
    tracing::error!(
        resource = %entry.name(),
        phase = %phase,
        failure = kind.label(),
        cause = %kind,
        "resource callback failed"
    );
}
