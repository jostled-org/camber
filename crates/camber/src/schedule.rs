use crate::RuntimeError;
use crate::runtime_state::LifecycleSignals;
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

    /// A fresh handle over `trigger`, and the cancel flag its loop observes.
    ///
    /// Both schedule constructors mint the pair the same way, so the handle
    /// can never watch a flag the loop does not read.
    fn paired(trigger: &Arc<tokio::sync::Notify>) -> (Self, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let handle = Self {
            cancelled: Arc::clone(&cancelled),
            trigger: Arc::clone(trigger),
        };
        (handle, cancelled)
    }
}

/// Schedule a closure to run repeatedly at `interval`.
///
/// The closure runs as a root-scope child, so runtime teardown awaits it.
/// The first invocation fires after one `interval` has elapsed. Respects
/// graceful shutdown — no new invocations fire after shutdown is requested
/// or the root scope closes.
///
/// Returns a `ScheduleHandle` that can cancel or trigger the task.
///
/// The closure is synchronous and runs inline on a Tokio worker. It has no
/// await point, so it cannot observe the root scope closing: a slow closure
/// holds its worker until it returns, and a closure still running at the
/// `shutdown_timeout` escalation boundary makes `runtime::run` return
/// `RuntimeError::ScopeDrainTimeout`.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is zero,
/// `RuntimeError::NoRuntime` if no runtime context is established — the
/// schedule is refused before any loop is built, never returned as an
/// inert handle — or `RuntimeError::ScopeClosed` if the root scope has
/// already closed to admission.
pub fn every<F>(interval: Duration, f: F) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() + Send + 'static,
{
    every_async(interval, move || {
        f();
        std::future::ready(())
    })
}

/// Schedule an async closure to run repeatedly at `interval`.
///
/// The first invocation fires after one `interval` has elapsed. Respects
/// graceful shutdown — no new invocations fire after shutdown is requested or
/// the root scope closes.
///
/// Returns a `ScheduleHandle` that can cancel or trigger the task.
///
/// `every_async` does not make the body stoppable. Only the *wait* between
/// invocations is raced against the lifecycle signals; the callback itself is
/// awaited unguarded, so an invocation already under way runs to completion
/// during a cooperative drain. What differs is only what happens to the body at
/// the escalation boundary: an async body is a Tokio task the drain can abort,
/// so it is dropped mid-await rather than left running. `runtime::run` returns
/// [`RuntimeError::ScopeDrainTimeout`] either way — being outstanding at the
/// boundary is what produces that result, not being unstoppable. Keep the body
/// short in either form.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is zero,
/// `RuntimeError::NoRuntime` if no runtime context is established — the
/// schedule is refused before any loop is built, never returned as an
/// inert handle — or `RuntimeError::ScopeClosed` if the root scope has
/// already closed to admission.
pub fn every_async<F, Fut>(interval: Duration, f: F) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    every_async_notified(interval, Arc::new(tokio::sync::Notify::new()), f)
}

/// Schedule an async closure with an external `Notify` as the trigger.
///
/// Both `handle.trigger()` and the external `notify.notify_one()` wake the
/// loop immediately. The first invocation fires after one `interval` elapses.
/// Respects graceful shutdown.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is zero,
/// `RuntimeError::NoRuntime` if no runtime context is established — the
/// schedule is refused before any loop is built, never returned as an
/// inert handle — or `RuntimeError::ScopeClosed` if the root scope has
/// already closed to admission.
pub fn every_async_notified<F, Fut>(
    interval: Duration,
    trigger: Arc<tokio::sync::Notify>,
    f: F,
) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    validate_interval(interval)?;
    let (handle, cancelled) = ScheduleHandle::paired(&trigger);
    crate::task::admit_signalled_loop(move |signals| {
        run_interval_async(cancelled, signals, trigger, interval, f)
    })?;
    Ok(handle)
}

/// Schedule a closure to run on a cron schedule.
///
/// Accepts standard 5-field cron expressions (e.g. `"*/5 * * * *"`).
/// A seconds field (`0`) is prepended automatically. Six or seven field
/// expressions are passed through as-is.
///
/// The closure runs on the Tokio runtime. Respects graceful shutdown.
/// Note: `trigger()` on the returned handle is a no-op for cron schedules.
///
/// The closure is synchronous and runs inline on a Tokio worker. It has no
/// await point, so it cannot observe the root scope closing: a slow closure
/// holds its worker until it returns, and a closure still running at the
/// `shutdown_timeout` escalation boundary makes `runtime::run` return
/// `RuntimeError::ScopeDrainTimeout`. Keep the body short, or hand long work
/// to something that can stop at an await point.
///
/// # Errors
///
/// Returns `RuntimeError::Schedule` if `expr` is not a valid cron expression,
/// or if it names no occurrence after now — a 7-field form pinned to a past
/// year parses cleanly and can never fire, so it is refused here rather than
/// handed back as a live handle over a dead loop. An expression that is finite
/// but still ahead cannot be caught at construction; the loop warns and stops
/// when it runs out, and the handle stays live.
///
/// Returns `RuntimeError::NoRuntime` if no runtime context is established — the
/// schedule is refused before any loop is built, never returned as an inert
/// handle — or `RuntimeError::ScopeClosed` if the root scope has already closed
/// to admission.
pub fn cron<F>(expr: &str, f: F) -> Result<ScheduleHandle, RuntimeError>
where
    F: Fn() + Send + 'static,
{
    let normalized = normalize_cron_expr(expr);
    let schedule: cron::Schedule = normalized
        .parse()
        .map_err(|e: cron::error::Error| RuntimeError::Schedule(e.to_string().into()))?;
    reject_exhausted(&schedule, expr)?;

    let trigger = Arc::new(tokio::sync::Notify::new());
    let (handle, cancelled) = ScheduleHandle::paired(&trigger);
    crate::task::admit_signalled_loop(move |signals| {
        run_cron(cancelled, signals, trigger, schedule, f)
    })?;
    Ok(handle)
}

fn should_stop(cancel: &AtomicBool, signals: &LifecycleSignals) -> bool {
    cancel.load(Ordering::Acquire) || signals.is_fired()
}

/// Why a schedule loop woke: its own due time, or an explicit `trigger()`.
///
/// Both loops decide whether to stop the same way, so the wake source is
/// carried out of `next_wake` instead of duplicating the stop check — and
/// then the resume in each arm — once per arm.
enum Wake {
    Due,
    Triggered,
}

/// Wait for the next wake, or `None` once this loop must stop.
///
/// `select!` is unbiased, so a wake completing in the same poll as a stop
/// request can win the race. Checking after the select — once, for every wake
/// source — makes the decision independent of that ordering.
///
/// The re-check is affordable HERE because a wake carries nothing: losing the
/// race costs one skipped invocation the caller never observed.
/// `LifecycleSignals::tick` re-checks for the same reason;
/// `LifecycleSignals::guard` deliberately does not, because a `work` arm that
/// won holds a completed `Fut::Output` that discarding would throw away.
///
/// Both schedule loops race the same three sources and stop on the same
/// predicate; only the due future and what follows a surviving wake differ. One
/// definition means the stop decision is made in one place and a second loop
/// cannot drift on what a wake means.
async fn next_wake(
    due: impl Future<Output = ()>,
    trigger: &tokio::sync::Notify,
    cancel: &AtomicBool,
    signals: &LifecycleSignals,
) -> Option<Wake> {
    let wake = tokio::select! {
        () = due => Wake::Due,
        () = trigger.notified() => Wake::Triggered,
        () = signals.wait() => return None,
    };
    match should_stop(cancel, signals) {
        true => None,
        false => Some(wake),
    }
}

/// The interval's own due time as a bare completion, so both loops hand
/// `next_wake` the same future shape.
async fn tick_due(tick: &mut tokio::time::Interval) {
    tick.tick().await;
}

async fn run_interval_async<F, Fut>(
    cancel: Arc<AtomicBool>,
    signals: LifecycleSignals,
    trigger: Arc<tokio::sync::Notify>,
    interval: Duration,
    f: F,
) where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let mut tick = tokio::time::interval(interval);
    // Delay, not the default Burst: a callback slower than `interval` would
    // otherwise make every missed period come due at once and fire the callback
    // back-to-back with no gap until it caught up. Every other perpetual Camber
    // loop is sleep-based and so already spaces its wakes by a full interval;
    // this is the one that had to ask for it.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // skip immediate first tick

    loop {
        let wake = match next_wake(tick_due(&mut tick), &trigger, &cancel, &signals).await {
            None => break,
            Some(wake) => wake,
        };
        // Unguarded on purpose. The wait above breaks on the lifecycle signals,
        // but an invocation that has already started runs to completion and the
        // drain waits for it: racing it here would drop a user callback halfway
        // through on every shutdown, including the clean ones. A body still
        // running at the escalation boundary is force-aborted there instead.
        f().await;
        // An early run replaces this period, so the next tick is one full
        // interval away rather than whatever was left of the current one.
        match wake {
            Wake::Triggered => tick.reset(),
            Wake::Due => {}
        }
    }
}

/// Fire `f` at each occurrence until the schedule is cancelled or a lifecycle
/// signal fires.
///
/// The trigger arm wakes the loop to re-evaluate its stop predicate and does
/// nothing else: `trigger()` is documented as a no-op for cron, so firing the
/// callback there would invent an occurrence the expression never names.
/// Without that arm a cancelled cron schedule would sleep on to its next
/// occurrence — up to a month — while the root scope retained its handle,
/// closure and cancel flag for the whole window.
///
/// A cron expression can name a finite set — a 7-field form pinned to a year is
/// the ordinary case. One already exhausted is refused by `reject_exhausted`
/// before any loop is built; one still ahead of now runs out here instead, and
/// only the loop can see that. Running out is not a stop anyone asked for: the
/// caller still holds a live `ScheduleHandle` for a loop that can never fire
/// again. Falling out silently is indistinguishable from a schedule that is
/// merely idle, so the exhausted source is reported, exactly as
/// `acme::report_event` reports an exhausted renewal stream.
async fn run_cron<F>(
    cancel: Arc<AtomicBool>,
    signals: LifecycleSignals,
    trigger: Arc<tokio::sync::Notify>,
    schedule: cron::Schedule,
    f: F,
) where
    F: Fn() + Send + 'static,
{
    loop {
        // One clock read serves both the occurrence lookup and the delay to it.
        // `upcoming` resolves against a `now` of its own, so reading the clock a
        // second time for the delta would make the sleep short by the gap
        // between the two reads and fire the callback before its own occurrence.
        let now = chrono::Utc::now();
        let next = match schedule.after(&now).next() {
            Some(next) => next,
            None => {
                // The expression is the only thing that says which schedule
                // died: several cron schedules in one process all end here, and
                // every handle stays live afterwards.
                tracing::warn!(
                    expr = schedule.source(),
                    "schedule: cron expression has no further occurrences; \
                     this schedule will never fire again"
                );
                break;
            }
        };
        let until = (next - now).to_std().unwrap_or(Duration::ZERO);

        let due = tokio::time::sleep(until);
        match next_wake(due, &trigger, &cancel, &signals).await {
            None => break,
            Some(Wake::Due) => f(),
            Some(Wake::Triggered) => {}
        }
    }
}

/// Refuse a cron expression that has already run out of occurrences.
///
/// The check is one lookup and the answer is available before anything is
/// built. Without it a past-pinned expression parses, claims a scope child, and
/// hands back a live handle for a loop whose first iteration finds nothing and
/// stops — the same shape `validate_interval` refuses for a zero interval.
fn reject_exhausted(schedule: &cron::Schedule, expr: &str) -> Result<(), RuntimeError> {
    match schedule.after(&chrono::Utc::now()).next() {
        Some(_) => Ok(()),
        None => Err(RuntimeError::Schedule(
            format!("cron expression `{expr}` has no future occurrences").into(),
        )),
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
