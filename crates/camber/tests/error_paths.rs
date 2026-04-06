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

#[test]
fn run_returns_error_on_invalid_config() {
    let result = camber::runtime::builder()
        .worker_threads(0)
        .run(|| "should not reach here");

    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("worker_threads"),
                "error should mention worker_threads, got: {msg}"
            );
        }
        Ok(_) => panic!("expected Err, got Ok"),
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
    }
}
