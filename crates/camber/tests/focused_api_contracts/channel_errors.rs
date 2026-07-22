use camber::RuntimeError;

#[test]
fn channel_send_after_receiver_drop_returns_error() {
    let (tx, rx) = camber::channel::new::<i32>();
    drop(rx);
    let result = tx.send(42);
    assert!(result.is_err());
    match result.unwrap_err() {
        RuntimeError::ChannelClosed => {}
        other => panic!("expected ChannelClosed, got: {other}"),
    }
}
