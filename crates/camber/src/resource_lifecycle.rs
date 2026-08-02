use crate::resource::{HealthState, Resource};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Shut down all registered resources in parallel.
/// Returned errors are logged without preventing siblings. The first callback
/// panic is returned after every shutdown thread has joined.
pub(crate) fn shutdown_resources(resources: &[Box<dyn Resource>]) -> Option<crate::RuntimeError> {
    let panic = std::sync::Mutex::new(None);
    std::thread::scope(|scope| spawn_shutdown_tasks(scope, resources, &panic));
    crate::runtime_state::recover_poisoned(panic.into_inner())
}

fn spawn_shutdown_tasks<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    resources: &'env [Box<dyn Resource>],
    panic: &'env std::sync::Mutex<Option<crate::RuntimeError>>,
) {
    for resource in resources.iter() {
        let resource = resource.as_ref();
        scope.spawn(move || shutdown_one(resource, panic));
    }
}

fn shutdown_one(resource: &dyn Resource, panic: &std::sync::Mutex<Option<crate::RuntimeError>>) {
    let mut name = None;
    let outcome = crate::task::catch_panic(|| {
        name = Some(resource.name());
        resource.shutdown()
    });
    let name = name.unwrap_or("<unnamed resource>");
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::error!(resource = name, %error, "resource shutdown failed");
        }
        Err(error) => record_shutdown_panic(name, error, panic),
    }
}

fn record_shutdown_panic(
    resource: &str,
    error: crate::RuntimeError,
    panic: &std::sync::Mutex<Option<crate::RuntimeError>>,
) {
    let mut first = crate::runtime_state::recover_poisoned(panic.lock());
    match first.as_ref() {
        Some(_) => tracing::warn!(resource, %error, "further resource shutdown panic"),
        None => {
            tracing::error!(resource, %error, "resource shutdown panicked");
            *first = Some(error);
        }
    }
}

/// Log a health check failure at warn level. Successes are silent.
fn log_health_result(name: &str, result: &Result<(), crate::RuntimeError>) {
    if let Err(e) = result {
        tracing::warn!(resource = name, error = %e, "health check failed");
    }
}

/// Admit one root-scope child per resource that periodically runs health
/// checks. Each child updates the corresponding AtomicBool in the health
/// state array and exits when either lifecycle signal fires, so runtime
/// teardown awaits it instead of aborting it.
///
/// The runtime is NAMED, not resolved from the ambient context: the scope that
/// awaits each loop and the signals it stops on then provably belong to the
/// same runtime, which is the one thing an admission built half from an `Arc`
/// and half from a thread-local could not promise.
pub(crate) fn admit_health_tasks(
    runtime: &Arc<crate::runtime_state::RuntimeInner>,
    resources: &Arc<[Box<dyn Resource>]>,
    health_state: &Option<HealthState>,
    interval: Duration,
) {
    let hs = match health_state {
        Some(hs) => hs,
        None => return,
    };

    for (idx, resource) in resources.iter().enumerate() {
        // The subsystem name is borrowed from the registry, so a refused
        // health loop is reported under its own resource's name.
        drop(crate::task::admit_signalled_subsystem_on(
            runtime,
            resource.name(),
            |signals| {
                run_health_task(
                    Arc::clone(resources),
                    Arc::clone(hs),
                    signals,
                    interval,
                    idx,
                )
            },
        ));
    }
}

/// Probe every resource once, before the user closure starts serving traffic.
///
/// The caller's runtime context is resolved here and carried into each probe:
/// a blocking worker starts with none of its own, where the interval probe
/// reaches its resource through `block_in_place` and keeps the context of the
/// task it runs on. Without this, `health_check` would see a Camber runtime on
/// every probe except the first.
pub(crate) async fn run_initial_health_checks(
    resources: &Arc<[Box<dyn Resource>]>,
    health_state: &HealthState,
) {
    let context = crate::runtime_state::try_current_runtime();
    let mut join_set = tokio::task::JoinSet::new();
    // Keyed by task id, because `join_next` alone cannot say which resource a
    // lost worker belonged to — and a probe that never reported must still be
    // named and marked, or `/health` keeps it on its optimistic seed.
    let mut probes = std::collections::HashMap::with_capacity(resources.len());
    for idx in 0..resources.len() {
        let handle = spawn_initial_health_check(
            &mut join_set,
            context.clone(),
            Arc::clone(resources),
            Arc::clone(health_state),
            idx,
        );
        // Task ids are unique, so there is never a displaced entry to consider.
        probes.insert(handle.id(), idx);
    }
    while let Some(joined) = join_set.join_next_with_id().await {
        report_probe_join(health_state, &probes, joined);
    }
}

/// Report a startup probe whose worker was lost.
///
/// The probe body catches its own unwind, so a `JoinError` here means the
/// blocking task itself never delivered — aborted, or cancelled with the join
/// set. Counting it and dropping it would leave that resource on the optimistic
/// seed `runtime` gave it, so `/health` would call it healthy on the strength of
/// a probe that never reported. A lost probe and a panicked one are the same
/// loss, so the task id names the resource and `record_probe_failure` gives both
/// one disposition. An id the map does not know is the only case left with no
/// resource to mark, and it is reported as itself.
fn report_probe_join(
    health_state: &HealthState,
    probes: &std::collections::HashMap<tokio::task::Id, usize>,
    joined: Result<(tokio::task::Id, ()), tokio::task::JoinError>,
) {
    let error = match joined {
        Ok(_) => return,
        Err(error) => error,
    };
    match probes.get(&error.id()).copied() {
        Some(idx) => record_probe_failure(health_state, idx, &probe_join_error(error)),
        None => tracing::error!(%error, "initial health check task failed to join"),
    }
}

/// Name what happened to a probe worker that never delivered.
///
/// A payload means the task unwound outside the body's own `catch_panic` — in
/// the context guard — and is reported as the panic it was. Everything else is
/// the join set stopping the task, which is a cancellation and not a fault of
/// the resource.
fn probe_join_error(error: tokio::task::JoinError) -> crate::RuntimeError {
    match error.try_into_panic() {
        Ok(payload) => crate::task::panic_to_error(payload),
        Err(_) => crate::RuntimeError::Cancelled,
    }
}

fn spawn_initial_health_check(
    join_set: &mut tokio::task::JoinSet<()>,
    context: Option<Arc<crate::runtime_state::RuntimeInner>>,
    resources: Arc<[Box<dyn Resource>]>,
    health_state: HealthState,
    idx: usize,
) -> tokio::task::AbortHandle {
    join_set.spawn_blocking(move || {
        // Absence stays absence: a probe launched with no context runs with
        // none, rather than being handed a minted runtime no owner awaits.
        let guard = context.map(crate::runtime_state::install_runtime);
        run_initial_probe(resources.as_ref(), &health_state, idx);
        drop(guard);
    })
}

/// Probe one resource on the startup pass, marking it unhealthy if the user's
/// `health_check` unwinds.
///
/// This pass owns no scope child and hands back no handle — the join set is
/// awaited for completion alone — so an uncaught unwind here would be swallowed
/// whole and leave the flag on its optimistic seed, reporting a resource healthy
/// on the strength of a probe that panicked. The interval pass keeps its own
/// disposition: it runs inside a scope-admitted child, where the scope records
/// the panic and `run` reports it.
fn run_initial_probe(resources: &[Box<dyn Resource>], health_state: &HealthState, idx: usize) {
    match crate::task::catch_panic(|| update_resource_health(resources, health_state, idx)) {
        Ok(()) => {}
        Err(error) => record_probe_failure(health_state, idx, &error),
    }
}

/// Publish a startup probe that never reported as a health failure, under the
/// resource's own name where the pair still has one.
///
/// Two probes reach here: one whose body unwound, and one whose worker was lost
/// before it could deliver. Both leave the flag on its optimistic seed unless
/// something writes it, so both get the same disposition and `error` says which
/// one it was.
fn record_probe_failure(health_state: &HealthState, idx: usize, error: &crate::RuntimeError) {
    match health_state.get(idx) {
        Some((name, healthy)) => {
            healthy.store(false, Ordering::Release);
            tracing::error!(resource = %name, %error, "initial health check did not report");
        }
        None => tracing::error!(idx, %error, "initial health check did not report"),
    }
}

/// Probe one resource every `interval` until either lifecycle signal fires.
///
/// Only the WAIT between probes breaks on the signals. The user's
/// `health_check` is synchronous and runs inline through `block_in_place`, so it
/// has no await point and cannot observe the root scope closing: a slow probe
/// holds its worker until it returns, and a probe still running at the
/// `shutdown_timeout` escalation boundary makes `runtime::run` return
/// `RuntimeError::ScopeDrainTimeout`. The same hazard `schedule::every` states
/// for its synchronous closure, and `Resource::health_check` names the bound its
/// implementors owe.
async fn run_health_task(
    resources: Arc<[Box<dyn Resource>]>,
    health_state: HealthState,
    signals: crate::runtime_state::LifecycleSignals,
    interval: Duration,
    idx: usize,
) {
    while let ControlFlow::Continue(()) = signals.tick(interval).await {
        // Flavor-checked: a bare `block_in_place` panics on a current-thread
        // runtime, and nothing in this file's types says which flavor admitted
        // the loop.
        crate::task::block_in_place(|| {
            update_resource_health(resources.as_ref(), &health_state, idx)
        });
    }
}

/// Probe the resource at `idx` and publish the outcome to its flag.
///
/// The registry and the health-state array are separate collections sharing one
/// index; nothing in their types ties their lengths. Indexing would turn a
/// disagreement into a panic inside a scope child, which displaces the whole
/// runtime's result — so the pair is resolved together and a mismatch is
/// reported as the wiring bug it is.
fn update_resource_health(resources: &[Box<dyn Resource>], health_state: &HealthState, idx: usize) {
    match (resources.get(idx), health_state.get(idx)) {
        (Some(resource), Some((_, healthy))) => probe_resource(resource.as_ref(), healthy),
        _ => tracing::error!(
            idx,
            resources = resources.len(),
            health_state = health_state.len(),
            "resource registry and health state disagree on length"
        ),
    }
}

fn probe_resource(resource: &dyn Resource, healthy: &AtomicBool) {
    let result = resource.health_check();
    log_health_result(resource.name(), &result);
    healthy.store(result.is_ok(), Ordering::Release);
}
