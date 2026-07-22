#![cfg(feature = "nats")]

use crate::common;
use crate::resources::{CleanupWitness, ExternalRun};

use camber::http::{Request, Response, Router};
use camber::mq::nats;
use camber::{RuntimeError, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

fn nats_url() -> String {
    std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".to_owned())
}

#[test]
#[ignore = "external lane nats; owner: Camber runtime integrations; run: gh workflow run external-evidence.yml -f lane=nats"]
fn nats_publish_and_subscribe() {
    let run = ExternalRun::from_environment().expect("valid CAMBER_EXTERNAL_RUN_ID");
    let witness =
        CleanupWitness::from_environment().expect("valid CAMBER_EXTERNAL_CLEANUP_WITNESS path");
    let subject = run.nats_subject("publish-and-subscribe");
    let url = nats_url();

    runtime::test(|| {
        let conn = nats::connect(&url).expect("connect");
        let mut sub = conn.subscribe(&subject).expect("subscribe");
        conn.publish(&subject, b"hello nats").expect("publish");
        let msg = sub.next_timeout(Duration::from_secs(2)).expect("receive");
        assert_eq!(msg.payload(), b"hello nats");
        drop(sub);
        drop(conn);
    })
    .unwrap();

    witness
        .emit(&run, &[&subject])
        .expect("emit cleanup witness after NATS resources drop");
}

#[test]
#[ignore = "external lane nats; owner: Camber runtime integrations; run: gh workflow run external-evidence.yml -f lane=nats"]
fn nats_queue_group() {
    let run = ExternalRun::from_environment().expect("valid CAMBER_EXTERNAL_RUN_ID");
    let witness =
        CleanupWitness::from_environment().expect("valid CAMBER_EXTERNAL_CLEANUP_WITNESS path");
    let subject = run.nats_subject("queue-group");
    let queue_group = run.nats_queue_group("queue-group");
    let url = nats_url();

    runtime::test(|| {
        let conn = nats::connect(&url).expect("connect");

        let counter_a = Arc::new(AtomicU32::new(0));
        let counter_b = Arc::new(AtomicU32::new(0));

        let mut sub_a = conn
            .queue_subscribe(&subject, &queue_group)
            .expect("subscribe a");
        let mut sub_b = conn
            .queue_subscribe(&subject, &queue_group)
            .expect("subscribe b");

        for i in 0..10 {
            conn.publish(&subject, format!("msg-{i}").as_bytes())
                .expect("publish");
        }

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

        ha.join().expect("subscriber a task").expect("subscriber a");
        hb.join().expect("subscriber b task").expect("subscriber b");

        let total = counter_a.load(Ordering::Relaxed) + counter_b.load(Ordering::Relaxed);
        assert_eq!(
            total, 10,
            "queue group should deliver each message exactly once"
        );
        drop(conn);
    })
    .unwrap();

    witness
        .emit(&run, &[&subject, &queue_group])
        .expect("emit cleanup witness after NATS resources drop");
}

#[test]
#[ignore = "external lane nats; owner: Camber runtime integrations; run: gh workflow run external-evidence.yml -f lane=nats"]
fn nats_async_subscribe() {
    let run = ExternalRun::from_environment().expect("valid CAMBER_EXTERNAL_RUN_ID");
    let witness =
        CleanupWitness::from_environment().expect("valid CAMBER_EXTERNAL_CLEANUP_WITNESS path");
    let subject = run.nats_subject("async-subscribe");
    let url = nats_url();

    runtime::test(|| {
        let conn = nats::connect(&url).expect("connect");
        let mut sub = conn.subscribe(&subject).expect("subscribe");
        conn.publish(&subject, b"async msg").expect("publish");
        let msg = sub.next_timeout(Duration::from_secs(2)).expect("receive");
        assert_eq!(msg.payload(), b"async msg");
        drop(sub);
        drop(conn);
    })
    .unwrap();

    witness
        .emit(&run, &[&subject])
        .expect("emit cleanup witness after NATS resources drop");
}

#[test]
#[ignore = "external lane nats; owner: Camber runtime integrations; run: gh workflow run external-evidence.yml -f lane=nats"]
fn nats_async_connect_does_not_block_worker() {
    const REQUEST_COUNT: usize = 5;

    let run = ExternalRun::from_environment().expect("valid CAMBER_EXTERNAL_RUN_ID");
    let witness =
        CleanupWitness::from_environment().expect("valid CAMBER_EXTERNAL_CLEANUP_WITNESS path");
    let subject: Arc<str> = run.nats_subject("async-worker").into();
    let url = nats_url();
    let nats_url: Arc<str> = url.into();

    common::test_runtime().run(|| {
        let (entered_sender, entered_receiver) = mpsc::channel();
        let overlap_barrier = Arc::new(tokio::sync::Barrier::new(REQUEST_COUNT));
        let mut router = Router::new();
        let url = Arc::clone(&nats_url);
        let route_subject = Arc::clone(&subject);
        router.get("/nats-async", move |_req: &Request| {
            let url = Arc::clone(&url);
            let subject = Arc::clone(&route_subject);
            let entered_sender = entered_sender.clone();
            let overlap_barrier = Arc::clone(&overlap_barrier);
            Box::pin(async move {
                let conn = match nats::connect_async(&url).await {
                    Ok(c) => c,
                    Err(e) => return Response::text(500, &format!("connect: {e}")),
                };
                let sub = match conn.subscribe_async(&subject).await {
                    Ok(s) => s,
                    Err(e) => return Response::text(500, &format!("subscribe: {e}")),
                };
                match conn.publish_async(&subject, b"ping").await {
                    Ok(()) => {}
                    Err(e) => return Response::text(500, &format!("publish: {e}")),
                };
                match entered_sender.send(()) {
                    Ok(()) => {}
                    Err(e) => return Response::text(500, &format!("evidence channel: {e}")),
                }
                match tokio::time::timeout(Duration::from_secs(5), overlap_barrier.wait()).await {
                    Ok(_) => {}
                    Err(e) => return Response::text(500, &format!("overlap barrier: {e}")),
                }
                drop(sub);
                drop(conn);
                Response::text(200, "ok")
            })
        });

        let addr = common::spawn_server(router);

        let handles: Vec<_> = (0..REQUEST_COUNT)
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

        (0..REQUEST_COUNT).for_each(|_| {
            entered_receiver
                .recv_timeout(Duration::from_secs(5))
                .expect("all request tasks entered the overlap barrier");
        });
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        for resp in &results {
            assert_eq!(
                crate::http::status_from_raw(resp),
                200,
                "expected 200, got: {resp}"
            );
        }

        runtime::request_shutdown();
    })
    .unwrap();

    witness
        .emit(&run, &[&subject])
        .expect("emit cleanup witness after NATS resources drop");
}
