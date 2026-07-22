use camber::RuntimeError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[camber::test]
async fn race_returns_first_to_complete() {
    let (fast_tx, fast_rx) = tokio::sync::oneshot::channel();
    let fast = async move {
        fast_rx.await.unwrap();
        "fast"
    };
    let slow = std::future::pending::<&str>();
    fast_tx.send(()).unwrap();

    assert_eq!(camber::task::race(fast, slow).await, "fast");
}

#[camber::test]
async fn race_all_returns_first_from_vec() {
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let (winner_tx, winner_rx) = tokio::sync::oneshot::channel();
    let winner_tx = std::sync::Mutex::new(Some(winner_tx));
    let mut releases = Vec::new();
    let futures = (0..5)
        .map(|index| {
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            releases.push(Some(release_tx));
            let task_ready = ready_tx.clone();
            let task_winner = &winner_tx;
            async move {
                task_ready.send(index).unwrap();
                release_rx.await.unwrap();
                match index {
                    2 => task_winner
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send(())
                        .unwrap(),
                    _ => {}
                }
                index
            }
        })
        .collect::<Vec<_>>();
    drop(ready_tx);

    let controller = tokio::spawn(async move {
        let mut observed = Vec::new();
        while let Some(index) = ready_rx.recv().await {
            observed.push(index);
            match observed.len() {
                5 => break,
                _ => {}
            }
        }
        releases[2].take().unwrap().send(()).unwrap();
        winner_rx.await.unwrap();
    });

    assert_eq!(camber::task::race_all(futures).await.unwrap(), 2);
    controller.await.unwrap();
}

#[camber::test]
async fn race_cancels_loser() {
    let dropped = Arc::new(AtomicBool::new(false));
    let drop_signal = DropSignal(Arc::clone(&dropped));
    let slow = async move {
        let drop_signal_guard = drop_signal;
        std::hint::black_box(&drop_signal_guard);
        std::future::pending::<&str>().await
    };

    assert_eq!(
        camber::task::race(std::future::ready("done"), slow).await,
        "done"
    );
    assert!(dropped.load(Ordering::Acquire));
}

#[camber::test]
async fn race_propagates_error_from_first() {
    let fast = std::future::ready(Err::<&str, RuntimeError>(RuntimeError::Timeout));
    let slow = std::future::pending::<Result<&str, RuntimeError>>();

    assert!(matches!(
        camber::task::race(fast, slow).await,
        Err(RuntimeError::Timeout)
    ));
}

#[camber::test]
async fn race_all_empty_returns_error() {
    let futures = Vec::<std::future::Ready<()>>::new();
    assert!(matches!(
        camber::task::race_all(futures).await,
        Err(RuntimeError::InvalidArgument(_))
    ));
}

#[camber::test]
async fn timeout_returns_error_on_expiry() {
    let result = camber::timeout(Duration::from_millis(50), std::future::pending::<()>()).await;
    assert!(matches!(result, Err(RuntimeError::Timeout)));
}

#[camber::test]
async fn timeout_returns_value_on_success() {
    assert_eq!(
        camber::timeout(Duration::from_secs(1), std::future::ready(42))
            .await
            .unwrap(),
        42
    );
}
