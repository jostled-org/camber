use crate::RuntimeError;
use crate::runtime;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Handle to a scheduled task. Call `cancel()` to stop it, `trigger()` to
/// wake it immediately.
#[derive(Debug, Clone)]
pub struct ScheduleHandle {
    cancelled: Arc<AtomicBool>,
    trigger: Arc<tokio::sync::Notify>,
}

impl ScheduleHandle {
    /// Stop the scheduled task from firing again.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.trigger.notify_one();
    }

    /// Wake the loop immediately, running the callback without waiting
    /// for the next interval tick.
    pub fn trigger(&self) {
        self.trigger.notify_one();
    }
}

/// Shared state for spawning a scheduled task.
struct ScheduleState {
    cancelled: Arc<AtomicBool>,
    trigger: Arc<tokio::sync::Notify>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
}

impl ScheduleState {
    fn new() -> Self {
        Self::with_trigger(Arc::new(tokio::sync::Notify::new()))
    }

    fn with_trigger(trigger: Arc<tokio::sync::Notify>) -> Self {
        let (shutdown, shutdown_notify) = runtime::shutdown_signal();
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            trigger,
            shutdown,
            shutdown_notify,
        }
    }

    fn handle(&self) -> ScheduleHandle {
        ScheduleHandle {
            cancelled: Arc::clone(&self.cancelled),
            trigger: Arc::clone(&self.trigger),
        }
    }
}

/// Schedule a closure to run repeatedly at `interval`.
///
/// The closure runs on the Tokio runtime. The first invocation fires
/// after one `interval` has elapsed. Respects graceful shutdown —
/// no new invocations fire after shutdown is requested.
///
/// Returns a `ScheduleHandle` that can cancel or trigger the task.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is zero.
pub fn every<F>(interval: Duration, f: F) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() + Send + Sync + 'static,
{
    every_async(interval, move || {
        f();
        std::future::ready(())
    })
}

/// Schedule an async closure to run repeatedly at `interval`.
///
/// The first invocation fires after one `interval` has elapsed. Respects
/// graceful shutdown — no new invocations fire after shutdown is requested.
///
/// Returns a `ScheduleHandle` that can cancel or trigger the task.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is zero.
pub fn every_async<F, Fut>(interval: Duration, f: F) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    validate_interval(interval)?;
    let state = ScheduleState::new();
    let handle = state.handle();
    tokio::spawn(run_interval_async(
        state.cancelled,
        state.shutdown,
        state.shutdown_notify,
        state.trigger,
        interval,
        f,
    ));
    Ok(handle)
}

/// Schedule an async closure with an external `Notify` as the trigger.
///
/// Both `handle.trigger()` and the external `notify.notify_one()` wake the
/// loop immediately. The first invocation fires after one `interval` elapses.
/// Respects graceful shutdown.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is zero.
pub fn every_async_notified<F, Fut>(
    interval: Duration,
    trigger: Arc<tokio::sync::Notify>,
    f: F,
) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    validate_interval(interval)?;
    let state = ScheduleState::with_trigger(trigger);
    let handle = state.handle();
    tokio::spawn(run_interval_async(
        state.cancelled,
        state.shutdown,
        state.shutdown_notify,
        state.trigger,
        interval,
        f,
    ));
    Ok(handle)
}

/// Schedule a closure to run on a cron schedule.
///
/// Accepts standard 5-field cron expressions (e.g. `"*/5 * * * *"`).
/// A seconds field (`0`) is prepended automatically. Six or seven field
/// expressions are passed through as-is.
///
/// Returns `Err` if the expression is invalid.
///
/// The closure runs on the Tokio runtime. Respects graceful shutdown.
/// Note: `trigger()` on the returned handle is a no-op for cron schedules.
pub fn cron<F>(expr: &str, f: F) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() + Send + 'static,
{
    let normalized = normalize_cron_expr(expr);
    let schedule: cron::Schedule = normalized
        .parse()
        .map_err(|e: cron::error::Error| RuntimeError::Schedule(e.to_string().into()))?;

    let state = ScheduleState::new();
    let handle = state.handle();
    tokio::spawn(run_cron(
        state.cancelled,
        state.shutdown,
        state.shutdown_notify,
        schedule,
        f,
    ));
    Ok(handle)
}

fn should_stop(cancel: &AtomicBool, shutdown: &AtomicBool) -> bool {
    cancel.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
}

async fn run_interval_async<F, Fut>(
    cancel: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    trigger: Arc<tokio::sync::Notify>,
    interval: Duration,
    f: F,
) where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // skip immediate first tick

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if should_stop(&cancel, &shutdown) { break; }
                f().await;
            }
            () = trigger.notified() => {
                if should_stop(&cancel, &shutdown) { break; }
                f().await;
                tick.reset();
            }
            () = shutdown_notify.notified() => break,
        }
    }
}

async fn run_cron<F>(
    cancel: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    schedule: cron::Schedule,
    f: F,
) where
    F: Fn() + Send + 'static,
{
    while let Some(next) = schedule.upcoming(chrono::Utc).next() {
        let until = (next - chrono::Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);

        tokio::select! {
            () = tokio::time::sleep(until) => {
                if should_stop(&cancel, &shutdown) { break; }
                f();
            }
            () = shutdown_notify.notified() => break,
        }
    }
}

fn validate_interval(interval: Duration) -> Result<(), RuntimeError> {
    match interval.is_zero() {
        true => Err(RuntimeError::InvalidArgument(
            "schedule interval must be non-zero".into(),
        )),
        false => Ok(()),
    }
}

/// Normalize a cron expression to the 6-field format the cron crate expects.
/// 5-field expressions (min hour dom month dow) get `0` prepended as seconds.
/// 6-field and 7-field expressions pass through unchanged.
fn normalize_cron_expr(expr: &str) -> std::borrow::Cow<'_, str> {
    let fields = expr.split_whitespace().count();
    match fields {
        5 => std::borrow::Cow::Owned(format!("0 {expr}")),
        _ => std::borrow::Cow::Borrowed(expr),
    }
}
