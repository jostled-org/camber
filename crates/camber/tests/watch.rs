use camber::RuntimeError;
use camber::channel::watch;
use std::time::Duration;

#[test]
fn initial_value_visible_to_receiver() {
    let (_tx, rx) = watch(42u32);
    assert_eq!(*rx.borrow(), 42);
}

#[test]
fn send_updates_receiver_value() {
    let (tx, rx) = watch(0u32);
    tx.send(7).unwrap();
    assert_eq!(*rx.borrow(), 7);
}

#[test]
fn multiple_sends_receiver_sees_latest() {
    let (tx, rx) = watch("first");
    tx.send("second").unwrap();
    tx.send("third").unwrap();
    assert_eq!(*rx.borrow(), "third");
}

#[test]
fn cloned_receivers_see_same_value() {
    let (tx, rx1) = watch(0u32);
    let rx2 = rx1.clone();
    tx.send(99).unwrap();
    assert_eq!(*rx1.borrow(), 99);
    assert_eq!(*rx2.borrow(), 99);
}

#[test]
fn send_after_all_receivers_dropped_returns_channel_closed() {
    let (tx, rx) = watch(0u32);
    drop(rx);
    let err = tx.send(1).unwrap_err();
    assert!(
        matches!(err, RuntimeError::ChannelClosed),
        "expected ChannelClosed, got {err:?}"
    );
}

#[test]
fn changed_resolves_on_new_value() {
    camber::runtime::test(|| {
        let (tx, mut rx) = watch(0u32);

        camber::spawn_async(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            tx.send(5).unwrap();
        });

        let result = camber::runtime::block_on(async { rx.changed().await });
        assert!(result.is_ok());
        assert_eq!(*rx.borrow(), 5);

        camber::runtime::request_shutdown();
    })
    .unwrap();
}

#[test]
fn changed_returns_channel_closed_when_sender_dropped() {
    camber::runtime::test(|| {
        let (tx, mut rx) = watch(0u32);
        drop(tx);

        let result = camber::runtime::block_on(async { rx.changed().await });
        assert!(
            matches!(result, Err(RuntimeError::ChannelClosed)),
            "expected ChannelClosed, got {result:?}"
        );

        camber::runtime::request_shutdown();
    })
    .unwrap();
}

#[test]
fn has_changed_tracks_seen_state() {
    let (tx, mut rx) = watch(0u32);
    // borrow_and_update marks as seen
    let _ = rx.borrow_and_update();
    assert!(!rx.has_changed());

    tx.send(1).unwrap();
    assert!(rx.has_changed());

    // borrow_and_update marks as seen again
    let _ = rx.borrow_and_update();
    assert!(!rx.has_changed());
}

#[test]
fn borrow_does_not_mark_as_seen() {
    let (tx, rx) = watch(0u32);
    tx.send(1).unwrap();

    // Plain borrow reads the value but does not clear has_changed
    let _ = rx.borrow();
    assert!(rx.has_changed());
}

#[test]
fn send_modify_updates_value() {
    let (tx, rx) = watch(vec![1, 2, 3]);
    tx.send_modify(|v| v.push(4));
    assert_eq!(&*rx.borrow(), &[1, 2, 3, 4]);
}

#[test]
fn send_modify_succeeds_after_receivers_dropped() {
    let (tx, rx) = watch(0u32);
    drop(rx);
    // send_modify always succeeds — the sender owns the value
    tx.send_modify(|v| *v = 1);
    assert_eq!(*tx.borrow(), 1);
}

#[test]
fn cloned_sender_writes_to_same_channel() {
    let (tx1, rx) = watch(0u32);
    let tx2 = tx1.clone();
    tx2.send(42).unwrap();
    assert_eq!(*rx.borrow(), 42);
    tx1.send(99).unwrap();
    assert_eq!(*rx.borrow(), 99);
}

#[test]
fn sender_borrow_reads_current_value() {
    let (tx, _rx) = watch(10u32);
    assert_eq!(*tx.borrow(), 10);
    tx.send(20).unwrap();
    assert_eq!(*tx.borrow(), 20);
}
