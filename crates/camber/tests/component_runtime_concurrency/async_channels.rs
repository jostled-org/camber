use camber::RuntimeError;

#[camber::test]
async fn mpsc_send_recv() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(16).unwrap();
    assert!(tx.try_send(1).is_ok());
    assert!(tx.try_send(2).is_ok());
    assert!(tx.try_send(3).is_ok());

    assert_eq!(rx.recv().await, Some(1));
    assert_eq!(rx.recv().await, Some(2));
    assert_eq!(rx.recv().await, Some(3));
}

#[camber::test]
async fn mpsc_sender_is_sync() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(16).unwrap();
    let tx2 = tx.clone();
    let first = camber::spawn(move || tx.send(1));
    let second = camber::spawn(move || tx2.send(2));

    let mut values = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
    values.sort_unstable();
    assert_eq!(values, vec![1, 2]);
    assert!(matches!(first.join(), Ok(Ok(()))));
    assert!(matches!(second.join(), Ok(Ok(()))));
}

#[camber::test]
async fn mpsc_try_send_full() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(2).unwrap();
    assert!(tx.try_send(1).is_ok());
    assert!(tx.try_send(2).is_ok());
    assert!(matches!(tx.try_send(3), Err(RuntimeError::ChannelFull)));

    assert_eq!(rx.recv().await, Some(1));
    assert!(tx.try_send(3).is_ok());
}

#[camber::test]
async fn mpsc_send_is_total_inside_async_runtime() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(1).unwrap();
    tx.try_send(1).unwrap();

    let received = camber::spawn_async(async move { (rx.recv().await, rx.recv().await) });
    let sent = tx.send(2);
    assert!(sent.is_ok(), "async-context send failed: {sent:?}");
    assert_eq!(received.await.unwrap(), (Some(1), Some(2)));
}

#[test]
fn mpsc_blocking_send_refuses_current_thread_async_context() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = camber::channel::mpsc::<u32>(1).unwrap();

    let result = runtime.block_on(async move { tx.send(1) });

    assert!(matches!(result, Err(RuntimeError::BlockingInAsyncContext)));
    drop(rx);
}

#[camber::test]
async fn mpsc_recv_returns_none_on_close() {
    let (tx, mut rx) = camber::channel::mpsc::<u32>(16).unwrap();
    drop(tx);
    assert_eq!(rx.recv().await, None);
}

#[test]
fn mpsc_zero_capacity_returns_error() {
    assert!(matches!(
        camber::channel::mpsc::<u32>(0),
        Err(RuntimeError::InvalidArgument(_))
    ));
}
