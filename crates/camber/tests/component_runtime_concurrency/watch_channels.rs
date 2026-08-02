use camber::RuntimeError;
use camber::channel::watch;

#[test]
fn initial_value_visible_to_receiver() {
    let (tx, rx) = watch(42_u32);
    assert_eq!(*rx.borrow(), 42);
    drop(tx);
}

#[test]
fn send_updates_receiver_value() {
    let (tx, rx) = watch(0_u32);
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
    let (tx, rx1) = watch(0_u32);
    let rx2 = rx1.clone();
    tx.send(99).unwrap();
    assert_eq!(*rx1.borrow(), 99);
    assert_eq!(*rx2.borrow(), 99);
}

#[test]
fn send_after_all_receivers_dropped_returns_channel_closed() {
    let (tx, rx) = watch(0_u32);
    drop(rx);
    assert!(matches!(tx.send(1), Err(RuntimeError::ChannelClosed)));
}

#[camber::test]
async fn changed_resolves_on_new_value() {
    let (tx, mut rx) = watch(0_u32);
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let sender_barrier = std::sync::Arc::clone(&barrier);
    camber::spawn_async(async move {
        sender_barrier.wait().await;
        tx.send(5).unwrap();
    });

    barrier.wait().await;
    assert!(matches!(rx.changed().await, Ok(())));
    assert_eq!(*rx.borrow(), 5);
}

#[camber::test]
async fn changed_returns_channel_closed_when_sender_dropped() {
    let (tx, mut rx) = watch(0_u32);
    drop(tx);

    assert!(matches!(
        rx.changed().await,
        Err(RuntimeError::ChannelClosed)
    ));
}

#[test]
fn has_changed_tracks_seen_state() {
    let (tx, mut rx) = watch(0_u32);
    drop(rx.borrow_and_update());
    assert!(!rx.has_changed().unwrap());

    tx.send(1).unwrap();
    assert!(rx.has_changed().unwrap());

    drop(rx.borrow_and_update());
    assert!(!rx.has_changed().unwrap());
}

#[test]
fn borrow_does_not_mark_as_seen() {
    let (tx, rx) = watch(0_u32);
    tx.send(1).unwrap();

    drop(rx.borrow());
    assert!(rx.has_changed().unwrap());
}

#[test]
fn has_changed_reports_sender_closure() {
    let (tx, rx) = watch(0_u32);
    drop(tx);

    assert!(matches!(rx.has_changed(), Err(RuntimeError::ChannelClosed)));
}

#[test]
fn send_modify_updates_value() {
    let (tx, rx) = watch(vec![1, 2, 3]);
    tx.send_modify(|value| value.push(4));
    assert_eq!(&*rx.borrow(), &[1, 2, 3, 4]);
}

#[test]
fn send_modify_succeeds_after_receivers_dropped() {
    let (tx, rx) = watch(0_u32);
    drop(rx);
    tx.send_modify(|value| *value = 1);
    assert_eq!(*tx.borrow(), 1);
}

#[test]
fn cloned_sender_writes_to_same_channel() {
    let (tx1, rx) = watch(0_u32);
    let tx2 = tx1.clone();
    tx2.send(42).unwrap();
    assert_eq!(*rx.borrow(), 42);
    tx1.send(99).unwrap();
    assert_eq!(*rx.borrow(), 99);
}

#[test]
fn sender_borrow_reads_current_value() {
    let (tx, rx) = watch(10_u32);
    assert_eq!(*tx.borrow(), 10);
    tx.send(20).unwrap();
    assert_eq!(*tx.borrow(), 20);
    drop(rx);
}
