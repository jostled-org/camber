use camber::RuntimeError;

#[camber::test]
async fn mpsc_send_recv() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(16).unwrap();
    tx.try_send(1).unwrap_or(());
    tx.try_send(2).unwrap_or(());
    tx.try_send(3).unwrap_or(());

    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(rx.recv().await, Some(2));
    assert_eq!(rx.recv().await, Some(3));
}

#[camber::test]
async fn mpsc_sender_is_sync() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(16).unwrap();
    let tx2 = tx.clone();

    camber::spawn(move || {
        if let Err(e) = tx.send(1) {
            eprintln!("send from task 1 failed: {e}");
        }
    });
    camber::spawn(move || {
        if let Err(e) = tx2.send(2) {
            eprintln!("send from task 2 failed: {e}");
        }
    });

    let mut values = vec![rx.recv().await.unwrap_or(0), rx.recv().await.unwrap_or(0)];
    values.sort();
    assert_eq!(values, vec![1, 2]);
}

#[camber::test]
async fn mpsc_try_send_full() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(2).unwrap();
    tx.try_send(1).unwrap_or(());
    tx.try_send(2).unwrap_or(());

    assert!(matches!(tx.try_send(3), Err(RuntimeError::ChannelFull)));

    // Drain one item, then try_send should succeed.
    assert_eq!(rx.recv().await, Some(1));
    tx.try_send(3).unwrap_or(());
}

#[camber::test]
async fn mpsc_recv_returns_none_on_close() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(16).unwrap();
    drop(tx);
    assert_eq!(rx.recv().await, None);
}

#[test]
fn mpsc_zero_capacity_returns_error() {
    let result = camber::channel::mpsc::<u32>(0);

    assert!(matches!(result, Err(RuntimeError::InvalidArgument(_))));
}
