use crate::resource::{HealthState, Resource};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// Shut down all registered resources in parallel.
/// Errors are logged but do not prevent others.
pub(crate) fn shutdown_resources(resources: &[Box<dyn Resource>]) {
    std::thread::scope(|scope| spawn_shutdown_tasks(scope, resources));
}

fn spawn_shutdown_tasks<'scope, 'env>(
    scope: &'scope std::thread::Scope<'scope, 'env>,
    resources: &'env [Box<dyn Resource>],
) {
    for resource in resources.iter() {
        let resource = resource.as_ref();
        scope.spawn(move || shutdown_one(resource));
    }
}

fn shutdown_one(resource: &dyn Resource) {
    if let Err(e) = resource.shutdown() {
        tracing::error!(resource = resource.name(), error = %e, "resource shutdown failed");
    }
}

/// Log a health check failure at warn level. Successes are silent.
fn log_health_result(name: &str, result: &Result<(), crate::RuntimeError>) {
    if let Err(e) = result {
        tracing::warn!(resource = name, error = %e, "health check failed");
    }
}

/// Spawn one background task per resource that periodically runs health checks.
/// Each task updates the corresponding AtomicBool in the health state array.
/// Tasks exit when shutdown is notified or when aborted.
pub(crate) fn spawn_health_tasks(
    resources: &Arc<[Box<dyn Resource>]>,
    health_state: &Option<HealthState>,
    interval: Duration,
    shutdown_notify: &Arc<tokio::sync::Notify>,
) -> Box<[tokio::task::JoinHandle<()>]> {
    let hs = match health_state {
        Some(hs) => hs,
        None => return Vec::new().into_boxed_slice(),
    };

    resources
        .iter()
        .enumerate()
        .map(|(idx, _)| {
            spawn_one_health_task(
                Arc::clone(resources),
                Arc::clone(hs),
                Arc::clone(shutdown_notify),
                interval,
                idx,
            )
        })
        .collect()
}

pub(crate) async fn run_initial_health_checks(
    resources: &Arc<[Box<dyn Resource>]>,
    health_state: &HealthState,
) {
    let mut join_set = tokio::task::JoinSet::new();
    for idx in 0..resources.len() {
        spawn_initial_health_check(
            &mut join_set,
            Arc::clone(resources),
            Arc::clone(health_state),
            idx,
        );
    }
    while join_set.join_next().await.is_some() {}
}

fn spawn_initial_health_check(
    join_set: &mut tokio::task::JoinSet<()>,
    resources: Arc<[Box<dyn Resource>]>,
    health_state: HealthState,
    idx: usize,
) {
    join_set.spawn_blocking(move || update_resource_health(resources.as_ref(), &health_state, idx));
}

fn spawn_one_health_task(
    resources: Arc<[Box<dyn Resource>]>,
    health_state: HealthState,
    shutdown_notify: Arc<tokio::sync::Notify>,
    interval: Duration,
    idx: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = shutdown_notify.notified() => return,
            }
            tokio::task::block_in_place(|| {
                update_resource_health(resources.as_ref(), &health_state, idx)
            });
        }
    })
}

fn update_resource_health(resources: &[Box<dyn Resource>], health_state: &HealthState, idx: usize) {
    let result = resources[idx].health_check();
    log_health_result(resources[idx].name(), &result);
    health_state[idx].1.store(result.is_ok(), Ordering::Release);
}
