use camber::{runtime, spawn};

#[test]
fn channel_sends_and_receives() {
    runtime::run(|| {
        let (tx, rx) = camber::channel::new::<i32>();

        spawn(move || {
            for i in 0..5 {
                assert!(tx.send(i).is_ok(), "channel send failed");
            }
        });

        let collected: Vec<i32> = rx.iter().collect();
        assert_eq!(collected, vec![0, 1, 2, 3, 4]);
    })
    .unwrap();
}

#[test]
fn bounded_channel_blocks_when_full() {
    runtime::run(|| {
        let (tx, rx) = camber::channel::bounded::<i32>(2);

        let sender = spawn(move || {
            for i in 0..3 {
                assert!(tx.send(i).is_ok(), "channel send failed");
            }
        });

        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut results = Vec::new();
        for _ in 0..3 {
            let val = rx.recv().expect("channel recv failed");
            results.push(val);
        }

        assert_eq!(results, vec![0, 1, 2]);
        sender.join().expect("sender thread panicked");
    })
    .unwrap();
}
