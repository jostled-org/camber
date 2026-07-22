use crate::schedule_probes::{AsyncProbe, SyncCallback, SyncProbe, async_deadline, sync_deadline};
use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::time::Duration;

const CALLBACK_INTERVAL: Duration = Duration::from_millis(25);
const INDEPENDENCE_INTERVAL: Duration = Duration::from_secs(60);
const MULTIPLE_SCHEDULES_MODE: &str = "runtime-multiple-schedules";
const MULTIPLE_SCHEDULES_MARKER: &str = "runtime-multiple-schedules-complete";
const MULTIPLE_SCHEDULES_TEST: &str = "periodic_scheduling::multiple_scheduled_tasks_independent";

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
async fn every_async_cancel_stops_task() {
    let (callback, mut probe) = AsyncProbe::new();
    let handle =
        camber::schedule::every_async(CALLBACK_INTERVAL, move || callback.clone().run()).unwrap();
    let deadline = async_deadline();

    probe.observe_and_release(deadline).await;
    probe.observe(deadline).await;
    handle.cancel();
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

#[camber::test]
async fn cancel_stops_loop() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let (callback, mut probe) = AsyncProbe::new();
    let handle = camber::schedule::every_async_notified(CALLBACK_INTERVAL, trigger, move || {
        callback.clone().run()
    })
    .unwrap();
    let deadline = async_deadline();

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
        assert!(camber::schedule::cron("not a cron", || {}).is_err());
        runtime::request_shutdown();
    })
    .unwrap();
}

#[test]
fn multiple_scheduled_tasks_independent() {
    match crate::process_support::is_private_child(MULTIPLE_SCHEDULES_MODE) {
        true => {
            assert_multiple_scheduled_tasks_independent();
            println!("{MULTIPLE_SCHEDULES_MARKER}");
            return;
        }
        false => {}
    }

    let run = crate::process_support::run_isolated_exact(
        MULTIPLE_SCHEDULES_TEST,
        MULTIPLE_SCHEDULES_MODE,
        MULTIPLE_SCHEDULES_MARKER,
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(
        run.success(),
        "isolated multiple-schedule contract failed: {}",
        String::from_utf8_lossy(run.stderr())
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
