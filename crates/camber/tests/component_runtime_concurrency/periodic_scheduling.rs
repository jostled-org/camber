use crate::common::{BOUND, wait_registry_at_most};
use crate::schedule_probes::{
    AsyncProbe, CALLBACK_INTERVAL, SyncCallback, SyncProbe, async_deadline, sync_deadline,
};
use crate::scope_builders::probed_runtime;
use camber::{RuntimeError, runtime};
use chrono::{Datelike, Timelike};
use std::sync::Arc;
use std::time::Duration;

const INDEPENDENCE_INTERVAL: Duration = Duration::from_secs(60);
const MULTIPLE_SCHEDULES_MODE: &str = "runtime-multiple-schedules";
const MULTIPLE_SCHEDULES_MARKER: &str = "runtime-multiple-schedules-complete";
const MULTIPLE_SCHEDULES_TEST: &str = "periodic_scheduling::multiple_scheduled_tasks_independent";

/// A 7-field cron expression pinned to a year that has already passed.
///
/// Fields are `sec min hour dom month dow year`. It parses cleanly and names no
/// instant after now, which is the pair of facts `reject_exhausted` exists for.
const EXHAUSTED_CRON: &str = "0 0 0 1 1 * 2000";

/// A 7-field cron expression whose only occurrence is decades out.
///
/// Far enough that a loop which slept to it rather than waking on its trigger
/// would never come back inside any bound this suite is willing to wait.
const DISTANT_CRON: &str = "0 0 0 1 1 * 2099";

/// How far ahead the single-occurrence expression is pinned.
///
/// Not a race window: production sleeps to the occurrence it was given, and
/// this is that occurrence. Short enough to keep the case quick, long enough
/// that the schedule is built and admitted well before it comes due.
const EXHAUSTION_LEAD: Duration = Duration::from_secs(2);

#[camber::test]
async fn every_async_fires_on_interval() {
    let (callback, mut probe) = AsyncProbe::new();
    let handle =
        camber::schedule::every_async(CALLBACK_INTERVAL, move || callback.clone().run()).unwrap();
    let deadline = async_deadline();

    probe.observe_and_release(deadline).await;
    probe.observe_and_release(deadline).await;
    probe.observe(deadline).await;
    handle.cancel();
    probe.release();
    probe.assert_disconnected(deadline).await;
}

#[camber::test]
async fn every_async_trigger_wakes_immediately() {
    let (callback, mut probe) = AsyncProbe::new();
    let handle =
        camber::schedule::every_async(Duration::from_secs(10), move || callback.clone().run())
            .unwrap();
    let deadline = async_deadline();

    handle.trigger();
    probe.observe(deadline).await;
    handle.cancel();
    probe.release();
    probe.assert_disconnected(deadline).await;
}

#[camber::test]
async fn every_async_stops_on_shutdown() {
    let (callback, mut probe) = AsyncProbe::new();
    camber::schedule::every_async(CALLBACK_INTERVAL, move || callback.clone().run()).unwrap();
    let deadline = async_deadline();

    probe.observe_and_release(deadline).await;
    probe.observe(deadline).await;
    runtime::request_shutdown();
    probe.release();
    probe.assert_disconnected(deadline).await;
}

#[camber::test]
async fn external_trigger_wakes_loop() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let (callback, mut probe) = AsyncProbe::new();
    let handle = camber::schedule::every_async_notified(
        Duration::from_secs(10),
        Arc::clone(&trigger),
        move || callback.clone().run(),
    )
    .unwrap();
    let deadline = async_deadline();

    trigger.notify_one();
    probe.observe(deadline).await;
    handle.cancel();
    probe.release();
    probe.assert_disconnected(deadline).await;
}

#[camber::test]
async fn handle_trigger_also_works() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let (callback, mut probe) = AsyncProbe::new();
    let handle =
        camber::schedule::every_async_notified(Duration::from_secs(10), trigger, move || {
            callback.clone().run()
        })
        .unwrap();
    let deadline = async_deadline();

    handle.trigger();
    probe.observe(deadline).await;
    handle.cancel();
    probe.release();
    probe.assert_disconnected(deadline).await;
}

#[camber::test]
async fn interval_still_fires() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let (callback, mut probe) = AsyncProbe::new();
    let handle = camber::schedule::every_async_notified(CALLBACK_INTERVAL, trigger, move || {
        callback.clone().run()
    })
    .unwrap();
    let deadline = async_deadline();

    probe.observe_and_release(deadline).await;
    probe.observe_and_release(deadline).await;
    probe.observe(deadline).await;
    handle.cancel();
    probe.release();
    probe.assert_disconnected(deadline).await;
}

#[test]
fn scheduled_task_fires_on_interval() {
    runtime::test(|| {
        let (callback, probe) = SyncProbe::new(());
        let handle = camber::schedule::every(CALLBACK_INTERVAL, move || callback.run()).unwrap();
        let deadline = sync_deadline();

        probe.observe_and_release(deadline);
        probe.observe_and_release(deadline);
        probe.observe(deadline);
        handle.cancel();
        probe.release();
        probe.assert_disconnected(deadline);
    })
    .unwrap();
}

#[test]
fn scheduled_task_stops_on_shutdown() {
    runtime::test(|| {
        let (callback, probe) = SyncProbe::new(());
        camber::schedule::every(CALLBACK_INTERVAL, move || callback.run()).unwrap();
        let deadline = sync_deadline();

        probe.observe_and_release(deadline);
        probe.observe(deadline);
        runtime::request_shutdown();
        probe.release();
        probe.assert_disconnected(deadline);
    })
    .unwrap();
}

#[test]
fn cron_expression_parsing() {
    runtime::test(|| {
        let handle = camber::schedule::cron("*/5 * * * *", || {});
        assert!(handle.is_ok(), "valid cron expression should parse");
        handle.unwrap().cancel();
        runtime::request_shutdown();
    })
    .unwrap();
}

#[test]
fn multiple_scheduled_tasks_independent() {
    crate::common::run_in_child(
        MULTIPLE_SCHEDULES_TEST,
        MULTIPLE_SCHEDULES_MODE,
        MULTIPLE_SCHEDULES_MARKER,
        BOUND,
        assert_multiple_scheduled_tasks_independent,
    );
}

fn assert_multiple_scheduled_tasks_independent() {
    runtime::test(|| {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (fast_callback, fast_release) = SyncCallback::new(event_tx.clone(), 100_u16);
        let (slow_callback, slow_release) = SyncCallback::new(event_tx, 200_u16);
        let fast =
            camber::schedule::every(INDEPENDENCE_INTERVAL, move || fast_callback.run()).unwrap();
        let slow =
            camber::schedule::every(INDEPENDENCE_INTERVAL, move || slow_callback.run()).unwrap();
        let deadline = sync_deadline();

        slow.trigger();
        assert_eq!(SyncProbe::receive(&event_rx, deadline), 200);
        fast.trigger();
        assert_eq!(SyncProbe::receive(&event_rx, deadline), 100);
        fast_release.release();
        fast.trigger();
        assert_eq!(SyncProbe::receive(&event_rx, deadline), 100);
        fast_release.release();
        slow_release.release();
        fast.cancel();
        slow.cancel();
        SyncProbe::assert_receiver_disconnected(&event_rx, deadline);
    })
    .unwrap();
}

#[test]
fn cron_parse_error_is_schedule_variant() {
    runtime::test(|| {
        let error = camber::schedule::cron("not a cron", || {}).unwrap_err();
        assert!(matches!(error, RuntimeError::Schedule(_)));
        assert!(error.to_string().contains("schedule"));
        runtime::request_shutdown();
    })
    .unwrap();
}

/// A cron expression that parses but names no future occurrence is refused at
/// construction, rather than handed back as a live handle over a loop whose
/// first lookup finds nothing.
#[test]
fn cron_pinned_to_a_past_year_is_refused_as_exhausted() {
    runtime::test(|| {
        let error = camber::schedule::cron(EXHAUSTED_CRON, || {}).unwrap_err();
        // Pinned to the sentence, not only the variant: a parse failure carries
        // the same variant, and a parse failure is the outcome this case must
        // not be quietly passing on.
        assert!(
            matches!(&error, RuntimeError::Schedule(message)
                if message.contains("no future occurrences")),
            "a past-pinned cron expression was not refused as exhausted: {error:?}"
        );
        runtime::request_shutdown();
    })
    .unwrap();
}

/// A cron expression still ahead of now at construction, but finite, runs out
/// inside its own loop: the loop stops itself and leaves the root scope while
/// the scope is still open, with nobody having cancelled it.
///
/// `reject_exhausted` cannot see this one — the occurrence is genuinely in the
/// future when the schedule is built — so the in-loop arm is the only thing
/// that ends it.
#[test]
fn cron_that_runs_out_mid_loop_stops_itself_and_leaves_the_scope() {
    let expr = single_occurrence_cron(EXHAUSTION_LEAD);
    let (fired_tx, fired_rx) = std::sync::mpsc::channel::<()>();
    let (controller, builder) = probed_runtime(BOUND);

    let handle = builder
        .run(move || {
            let before = scope_entries(&controller);
            let handle = camber::schedule::cron(&expr, move || {
                let _ = fired_tx.send(());
            })
            .unwrap();
            assert_eq!(
                scope_entries(&controller),
                before + 1,
                "the cron loop was not admitted as a root-scope child"
            );

            fired_rx
                .recv_timeout(BOUND)
                .expect("the pinned cron occurrence never fired");
            // Ordering, not budget: nothing else in the scope can exit while
            // this closure runs — only `ScopeClosing` ends the runtime's own
            // children, and that has not fired — so the registry falling back
            // to its pre-admission length IS the exhaustion break.
            assert!(
                wait_registry_at_most(&controller, before, BOUND),
                "the exhausted cron loop stayed in the root scope after its last occurrence"
            );
            handle
        })
        .unwrap();

    // Nothing cancelled the loop: it stopped because its expression ran out,
    // and the caller is left holding a live handle over a schedule that can
    // never fire again — the reason that arm warns instead of falling out
    // silently. The handle still accepts this, now as a no-op.
    handle.cancel();
}

/// Cancelling a cron schedule wakes its loop instead of leaving it asleep until
/// its next occurrence.
///
/// That is what `run_cron`'s trigger arm is for: the expression below names an
/// instant decades out, and a loop that slept to it would hold its scope entry
/// — and the closure and cancel flag with it — for the whole window.
#[test]
fn cancelled_cron_wakes_instead_of_sleeping_to_its_next_occurrence() {
    let (controller, builder) = probed_runtime(BOUND);

    builder
        .run(move || {
            let before = scope_entries(&controller);
            let handle = camber::schedule::cron(DISTANT_CRON, || {}).unwrap();
            assert_eq!(
                scope_entries(&controller),
                before + 1,
                "the cron loop was not admitted as a root-scope child"
            );

            handle.cancel();
            assert!(
                wait_registry_at_most(&controller, before, BOUND),
                "the cancelled cron loop slept on toward its next occurrence"
            );
        })
        .unwrap();
}

/// How many children the root scope retains right now.
fn scope_entries(controller: &camber::runtime_test_support::RuntimeController) -> usize {
    controller
        .scope_registry_len()
        .expect("the runtime schedule could not read the root scope registry")
}

/// A 7-field cron expression naming exactly one occurrence, `lead` from now.
///
/// Every field is fixed to that one instant, the year included, so the
/// expression names that second and nothing after it. `reject_exhausted`
/// accepts it, the loop fires once, and its next lookup finds nothing — which
/// is the arm under proof.
fn single_occurrence_cron(lead: Duration) -> Box<str> {
    let fire = chrono::Utc::now() + lead;
    format!(
        "{} {} {} {} {} * {}",
        fire.second(),
        fire.minute(),
        fire.hour(),
        fire.day(),
        fire.month(),
        fire.year()
    )
    .into_boxed_str()
}
