use std::time::Duration;

use crate::schedule_probes::{AsyncProbe, async_deadline};

#[camber::test]
async fn test_macro_runs_async_body() {
    assert_eq!(1 + 1, 2);
}

#[camber::test]
async fn test_macro_supports_spawn_async() {
    assert_eq!(camber::spawn_async(async { 42 }).await.unwrap(), 42);
}

#[camber::test]
async fn test_macro_supports_schedule() {
    let (callback, mut probe) = AsyncProbe::new();
    let handle =
        camber::schedule::every_async(Duration::from_millis(25), move || callback.clone().run())
            .unwrap();
    let deadline = async_deadline();

    probe.observe(deadline).await;
    handle.cancel();
    probe.release();
    probe.assert_disconnected(deadline).await;
}
