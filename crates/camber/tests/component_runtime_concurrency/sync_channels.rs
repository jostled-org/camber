use camber::{channel, runtime, spawn};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[test]
fn channel_sends_and_receives() {
    runtime::run(|| {
        let (tx, rx) = channel::new::<i32>();
        spawn(move || (0..5).try_for_each(|value| tx.send(value)));

        assert_eq!(rx.iter().collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
    })
    .unwrap();
}

#[test]
fn bounded_channel_blocks_when_full() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(2);
        let (attempting_tx, attempting_rx) = std::sync::mpsc::sync_channel(0);
        let (sent_tx, sent_rx) = std::sync::mpsc::sync_channel(0);

        let sender = spawn(move || {
            tx.send(0).unwrap();
            tx.send(1).unwrap();
            attempting_tx.send(()).unwrap();
            tx.send(2).unwrap();
            sent_tx.send(()).unwrap();
        });

        attempting_rx.recv().unwrap();
        assert!(matches!(
            sent_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));

        assert_eq!(rx.recv().unwrap(), 0);
        sent_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(rx.recv().unwrap(), 1);
        assert_eq!(rx.recv().unwrap(), 2);
        sender.join().unwrap();
    })
    .unwrap();
}

#[test]
fn mpmc_multiple_consumers() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<usize>(10);
        let received = Arc::new(AtomicUsize::new(0));
        let handles = (0..3)
            .map(|_| {
                let consumer_rx = rx.clone();
                let counter = Arc::clone(&received);
                spawn(move || {
                    consumer_rx
                        .iter()
                        .inspect(|_| {
                            counter.fetch_add(1, Ordering::SeqCst);
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        drop(rx);

        (0..30).try_for_each(|value| tx.send(value)).unwrap();
        drop(tx);

        let mut all_values = HashSet::new();
        handles.into_iter().for_each(|handle| {
            handle.join().unwrap().into_iter().for_each(|value| {
                assert!(all_values.insert(value), "duplicate value: {value}");
            });
        });

        assert_eq!(all_values.len(), 30);
        assert_eq!(received.load(Ordering::SeqCst), 30);
        assert!((0..30).all(|value| all_values.contains(&value)));
    })
    .unwrap();
}

#[test]
fn select_picks_ready_channel() {
    runtime::run(|| {
        let (fast_tx, fast_rx) = channel::bounded::<&str>(1);
        let (slow_tx, slow_rx) = channel::bounded::<&str>(1);
        fast_tx.send("fast").unwrap();

        let value = camber::select! {
            value = fast_rx => value.unwrap(),
            value = slow_rx => value.unwrap(),
        };

        assert_eq!(value, "fast");
        drop(slow_tx);
    })
    .unwrap();
}

#[test]
fn select_with_timeout() {
    runtime::run(|| {
        let (sender, rx) = channel::bounded::<i32>(1);
        let start = Instant::now();
        let timed_out = camber::select! {
            _value = rx => false,
            timeout(Duration::from_millis(100)) => true,
        };
        let elapsed = start.elapsed();

        assert!(timed_out, "expected timeout arm to trigger");
        assert!(
            elapsed >= Duration::from_millis(90),
            "timeout too fast: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout too slow: {elapsed:?}"
        );
        drop(sender);
    })
    .unwrap();
}

#[test]
fn existing_channel_api_unchanged() {
    runtime::run(|| {
        let (tx_str, rx_str) = channel::new::<String>();
        tx_str.send("hello".to_owned()).unwrap();
        assert_eq!(rx_str.recv().unwrap(), "hello");

        let (tx_i32, rx_i32) = channel::bounded::<i32>(10);
        tx_i32.send(42).unwrap();
        tx_i32.send(43).unwrap();
        assert_eq!(rx_i32.recv().unwrap(), 42);
        assert_eq!(rx_i32.recv().unwrap(), 43);

        let (tx_iter, rx_iter) = channel::bounded::<i32>(5);
        spawn(move || (0..3).try_for_each(|value| tx_iter.send(value)));
        assert_eq!(rx_iter.iter().collect::<Vec<_>>(), vec![0, 1, 2]);
    })
    .unwrap();
}

#[test]
fn channel_recv_returns_immediately_when_data_available() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(10);
        tx.send(42).unwrap();
        let start = Instant::now();

        assert_eq!(rx.recv().unwrap(), 42);
        let elapsed = start.elapsed();

        // A ready Crossbeam receive is in-thread work. This 25 ms safety bound is
        // generous under CI preemption while still rejecting scheduler polling.
        assert!(
            elapsed < Duration::from_millis(25),
            "ready receive polled or stalled for {elapsed:?}"
        );
    })
    .unwrap();
}

/// The macro must resolve its own Crossbeam path rather than requiring an import.
#[test]
fn select_macro_works_without_crossbeam_in_scope() {
    runtime::run(|| {
        let (tx1, rx1) = channel::bounded::<i32>(1);
        let (tx2, rx2) = channel::bounded::<i32>(1);
        tx1.send(10).unwrap();
        tx2.send(20).unwrap();

        let value = camber::select! {
            value = rx1 => value.unwrap(),
            value = rx2 => value.unwrap(),
        };
        assert!(matches!(value, 10 | 20));

        let (tx3, rx3) = channel::bounded::<i32>(1);
        let timed_out = camber::select! {
            value = rx3 => { drop(value); false },
            timeout(Duration::from_millis(50)) => true,
        };
        assert!(timed_out);
        drop(tx3);
    })
    .unwrap();
}
