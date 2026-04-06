#![cfg(feature = "nats")]

mod common;

use camber::http::{Request, Response, Router};
use camber::mq::nats;
use camber::{RuntimeError, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

fn nats_url() -> Option<String> {
    std::env::var("NATS_URL").ok().or_else(|| {
        // Try default local nats-server
        Some("nats://127.0.0.1:4222".to_owned())
    })
}

fn skip_if_no_nats() -> Option<String> {
    let url = nats_url()?;
    // Quick connect check — skip test if nats-server isn't running
    let result = std::panic::catch_unwind(|| runtime::test(|| nats::connect(&url).ok()).unwrap());
    match result {
        Ok(Some(_)) => Some(url),
        _ => {
            eprintln!("NATS server not available at {url}, skipping test");
            None
        }
    }
}

#[test]
fn nats_publish_and_subscribe() {
    let url = match skip_if_no_nats() {
        Some(u) => u,
        None => return,
    };

    runtime::test(|| {
        let conn = nats::connect(&url).expect("connect");

        let mut sub = conn.subscribe("camber.test.pubsub").expect("subscribe");

        conn.publish("camber.test.pubsub", b"hello nats")
            .expect("publish");

        let msg = sub.next_timeout(Duration::from_secs(2)).expect("receive");
        assert_eq!(msg.payload(), b"hello nats");
    })
    .unwrap();
}

#[test]
fn nats_queue_group() {
    let url = match skip_if_no_nats() {
        Some(u) => u,
        None => return,
    };

    runtime::test(|| {
        let conn = nats::connect(&url).expect("connect");

        let counter_a = Arc::new(AtomicU32::new(0));
        let counter_b = Arc::new(AtomicU32::new(0));

        let mut sub_a = conn
            .queue_subscribe("camber.test.queue", "workers")
            .expect("subscribe a");
        let mut sub_b = conn
            .queue_subscribe("camber.test.queue", "workers")
            .expect("subscribe b");

        // Drain any stale messages first
        while sub_a.try_next().is_some() {}
        while sub_b.try_next().is_some() {}

        // Publish 10 messages
        for i in 0..10 {
            conn.publish("camber.test.queue", format!("msg-{i}").as_bytes())
                .expect("publish");
        }

        // Collect with timeout
        let ca = Arc::clone(&counter_a);
        let cb = Arc::clone(&counter_b);

        let ha = spawn(move || -> Result<(), RuntimeError> {
            loop {
                match sub_a.next_timeout(Duration::from_millis(500)) {
                    Ok(_) => {
                        ca.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        });

        let hb = spawn(move || -> Result<(), RuntimeError> {
            loop {
                match sub_b.next_timeout(Duration::from_millis(500)) {
                    Ok(_) => {
                        cb.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => break,
                }
            }
            Ok(())
        });

        let _ = ha.join();
        let _ = hb.join();

        let total = counter_a.load(Ordering::Relaxed) + counter_b.load(Ordering::Relaxed);
        assert_eq!(
            total, 10,
            "queue group should deliver each message exactly once"
        );
    })
    .unwrap();
}

#[test]
fn nats_async_subscribe() {
    let url = match skip_if_no_nats() {
        Some(u) => u,
        None => return,
    };

    runtime::test(|| {
        let conn = nats::connect(&url).expect("connect");

        let mut sub = conn.subscribe("camber.test.async").expect("subscribe");

        conn.publish("camber.test.async", b"async msg")
            .expect("publish");

        let msg = sub.next_timeout(Duration::from_secs(2)).expect("receive");
        assert_eq!(msg.payload(), b"async msg");
    })
    .unwrap();
}

#[test]
fn nats_async_connect_does_not_block_worker() {
    let url = match skip_if_no_nats() {
        Some(u) => u,
        None => return,
    };

    let nats_url: Arc<str> = url.into();

    common::test_runtime().run(|| {
        let mut router = Router::new();
        let url = Arc::clone(&nats_url);
        router.get("/nats-async", move |_req: &Request| {
            let url = Arc::clone(&url);
            Box::pin(async move {
                let conn = match nats::connect_async(&url).await {
                    Ok(c) => c,
                    Err(e) => return Response::text(500, &format!("connect: {e}")),
                };
                let _sub = match conn.subscribe_async("camber.test.asyncworker").await {
                    Ok(s) => s,
                    Err(e) => return Response::text(500, &format!("subscribe: {e}")),
                };
                match conn.publish_async("camber.test.asyncworker", b"ping").await {
                    Ok(()) => {}
                    Err(e) => return Response::text(500, &format!("publish: {e}")),
                };
                // Sleep to make concurrent vs serial measurable
                tokio::time::sleep(Duration::from_millis(50)).await;
                Response::text(200, "ok")
            })
        });

        let addr = common::spawn_server(router);

        let start = std::time::Instant::now();
        let handles: Vec<_> = (0..5)
            .map(|_| {
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    let mut stream = std::net::TcpStream::connect(addr).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .write_all(
                            b"GET /nats-async HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                    let mut buf = String::new();
                    stream.read_to_string(&mut buf).unwrap();
                    buf
                })
            })
            .collect();

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let elapsed = start.elapsed();

        for resp in &results {
            assert!(
                resp.starts_with("HTTP/1.1 200"),
                "expected 200, got: {resp}"
            );
        }

        // 5 requests each sleeping 50ms: serial ≈ 250ms+, concurrent < 500ms
        assert!(
            elapsed < Duration::from_millis(500),
            "concurrent async nats ops took {elapsed:?}, expected < 500ms"
        );

        runtime::request_shutdown();
    }).unwrap();
}
