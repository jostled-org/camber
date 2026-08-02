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

const ASYNC_REQUEST_COUNT: usize = 5;

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

fn async_nats_router(
    nats_url: Arc<str>,
    subject: Arc<str>,
    entered_sender: mpsc::Sender<()>,
    overlap_barrier: Arc<tokio::sync::Barrier>,
) -> Router {
    let mut router = Router::new();
    router.get("/nats-async", move |_req: &Request| {
        let url = Arc::clone(&nats_url);
        let subject = Arc::clone(&subject);
        let entered_sender = entered_sender.clone();
        let overlap_barrier = Arc::clone(&overlap_barrier);
        Box::pin(async move {
            let conn = match nats::connect_async(&url).await {
                Ok(connection) => connection,
                Err(error) => return Response::text(500, &format!("connect: {error}")),
            };
            let subscription = match conn.subscribe_async(&subject).await {
                Ok(subscription) => subscription,
                Err(error) => return Response::text(500, &format!("subscribe: {error}")),
            };
            match conn.publish_async(&subject, b"ping").await {
                Ok(()) => {}
                Err(error) => return Response::text(500, &format!("publish: {error}")),
            };
            match entered_sender.send(()) {
                Ok(()) => {}
                Err(error) => {
                    return Response::text(500, &format!("evidence channel: {error}"));
                }
            }
            match tokio::time::timeout(Duration::from_secs(5), overlap_barrier.wait()).await {
                Ok(_) => {}
                Err(error) => {
                    return Response::text(500, &format!("overlap barrier: {error}"));
                }
            }
            drop(subscription);
            drop(conn);
            Response::text(200, "ok")
        })
    });
    router
}

fn spawn_async_nats_requests(addr: std::net::SocketAddr) -> Vec<std::thread::JoinHandle<Box<str>>> {
    (0..ASYNC_REQUEST_COUNT)
        .map(|_| {
            std::thread::spawn(move || crate::http::raw_request(addr, "GET", "/nats-async", &[]))
        })
        .collect()
}

fn wait_for_async_overlap(entered_receiver: &mpsc::Receiver<()>) {
    (0..ASYNC_REQUEST_COUNT).for_each(|_| {
        entered_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("all request tasks entered the overlap barrier");
    });
}

fn assert_async_nats_responses(handles: Vec<std::thread::JoinHandle<Box<str>>>) {
    let results: Box<[Box<str>]> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    for response in &results {
        assert_eq!(
            crate::http::status_from_raw(response),
            200,
            "expected 200, got: {response}"
        );
    }
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
    let run = ExternalRun::from_environment().expect("valid CAMBER_EXTERNAL_RUN_ID");
    let witness =
        CleanupWitness::from_environment().expect("valid CAMBER_EXTERNAL_CLEANUP_WITNESS path");
    let subject: Arc<str> = run.nats_subject("async-worker").into();
    let url = nats_url();
    let nats_url: Arc<str> = url.into();

    common::test_runtime()
        .run(|| {
            let (entered_sender, entered_receiver) = mpsc::channel();
            let overlap_barrier = Arc::new(tokio::sync::Barrier::new(ASYNC_REQUEST_COUNT));
            let router = async_nats_router(
                Arc::clone(&nats_url),
                Arc::clone(&subject),
                entered_sender,
                overlap_barrier,
            );
            let addr = common::spawn_server(router);
            let handles = spawn_async_nats_requests(addr);
            wait_for_async_overlap(&entered_receiver);
            assert_async_nats_responses(handles);

            runtime::request_shutdown();
        })
        .unwrap();

    witness
        .emit(&run, &[&subject])
        .expect("emit cleanup witness after NATS resources drop");
}
