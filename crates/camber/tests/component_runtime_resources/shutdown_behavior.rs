use std::future::IntoFuture;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use camber::http::mock::{LifecycleCheckpoint, lifecycle};
use camber::http::{Request, Response, Router};
use futures_util::FutureExt;

use crate::common::{ChildGuard, assert_admission_closed, is_private_child};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const IMMEDIATE_BOUNDARY: Duration = Duration::from_millis(100);

#[camber::test]
async fn shutdown_stops_accepting_immediately() {
    let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
    let entered_sender = Arc::new(std::sync::Mutex::new(Some(entered_sender)));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_release = Arc::clone(&release);
    let mut router = Router::new();
    router.get("/hello", |_request: &Request| async {
        Response::text(200, "Hello")
    });
    router.get("/held", move |_request: &Request| {
        let entered_sender = Arc::clone(&entered_sender);
        let release = Arc::clone(&handler_release);
        async move {
            if let Some(sender) = entered_sender
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
            let permit = release.acquire_owned().await.unwrap();
            permit.forget();
            Response::text(200, "held")
        }
    });
    let listener = crate::common::BoundListener::bind_tcp("127.0.0.1:0").unwrap();
    let addr = listener.local_addr();
    let controller = lifecycle(addr).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    let server = crate::common::ReadyServer::start(listener, router, EVENT_TIMEOUT).unwrap();
    let response = reqwest::get(format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(response.status(), 200);
    let held_client = tokio::spawn(reqwest::get(format!("http://{addr}/held")));
    tokio::time::timeout(EVENT_TIMEOUT, entered_receiver)
        .await
        .unwrap()
        .unwrap();

    let started = Instant::now();
    camber::runtime::request_shutdown();
    wait_for_checkpoint(&controller, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    wait_for_checkpoint(&controller, LifecycleCheckpoint::BeforeSupervisorSelect).await;
    let checkpoint_elapsed = started.elapsed();
    assert_admission_closed(addr, EVENT_TIMEOUT).await;
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    release.add_permits(1);
    let held_response = tokio::time::timeout(EVENT_TIMEOUT, held_client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let server_result = server.shutdown_bounded(EVENT_TIMEOUT);

    assert!(
        checkpoint_elapsed < IMMEDIATE_BOUNDARY,
        "runtime shutdown reached the admission-stop checkpoint in {checkpoint_elapsed:?}"
    );
    assert_eq!(held_response.status(), 200);
    assert!(
        server_result.is_ok(),
        "owned server returned {server_result:?}"
    );
}

async fn wait_for_checkpoint(
    controller: &camber::http::mock::LifecycleController,
    checkpoint: LifecycleCheckpoint,
) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap()
        .unwrap();
}

#[camber::test]
async fn shutdown_drains_inflight_requests() {
    let completed = Arc::new(AtomicBool::new(false));
    let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
    let entered_sender = Arc::new(std::sync::Mutex::new(Some(entered_sender)));
    let release = Arc::new(tokio::sync::Semaphore::new(0));

    let mut router = Router::new();
    router.get("/slow", {
        let completed = Arc::clone(&completed);
        let entered_sender = Arc::clone(&entered_sender);
        let release = Arc::clone(&release);
        move |_request: &Request| {
            let completed = Arc::clone(&completed);
            let entered_sender = Arc::clone(&entered_sender);
            let release = Arc::clone(&release);
            async move {
                if let Some(sender) = entered_sender
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take()
                {
                    let _ = sender.send(());
                }
                let permit = release.acquire_owned().await.unwrap();
                permit.forget();
                completed.store(true, Ordering::Release);
                Response::text(200, "done")
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    let client = tokio::spawn(reqwest::get(format!("http://{addr}/slow")));
    tokio::time::timeout(EVENT_TIMEOUT, entered_receiver)
        .await
        .unwrap()
        .unwrap();

    camber::runtime::request_shutdown();
    let mut owner = Box::pin(handle.into_future());
    assert!(owner.as_mut().now_or_never().is_none());
    release.add_permits(1);
    let response = tokio::time::timeout(EVENT_TIMEOUT, client)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "done");
    assert!(completed.load(Ordering::Acquire));
    assert!(
        tokio::time::timeout(EVENT_TIMEOUT, owner)
            .await
            .unwrap()
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn sigterm_triggers_shutdown() {
    const MODE: &str = "phase5-runtime-sigterm";
    const READY: &str = "PHASE5_RUNTIME_SIGTERM_READY";
    const MARKER: &str = "PHASE5_RUNTIME_SIGTERM_DRAINED";
    const TEST_NAME: &str = "shutdown_behavior::sigterm_triggers_shutdown";

    if is_private_child(MODE) {
        let mut router = Router::new();
        router.get("/hello", |_request: &Request| async {
            Response::text(200, "ok")
        });
        let (server_result, mut probe) = camber::runtime::builder()
            .run(move || {
                let listener = camber::net::listen("127.0.0.1:0").unwrap();
                let addr = listener.local_addr().unwrap().tcp().unwrap();
                let probe = std::thread::spawn(move || {
                    let response =
                        crate::common::request(addr, "GET", "/hello", &[], &[], EVENT_TIMEOUT)
                            .unwrap();
                    assert_eq!(response.status, 200);
                    println!("{READY}");
                    std::io::stdout().flush().unwrap();
                });
                (camber::http::serve_listener(listener, router), Some(probe))
            })
            .unwrap();
        crate::common::join_thread_bounded(&mut probe, EVENT_TIMEOUT).unwrap();
        assert!(
            server_result.is_ok(),
            "signal-owned server failed: {server_result:?}"
        );
        println!("{MARKER}");
        std::io::stdout().flush().unwrap();
        return;
    }

    let mut child = ChildGuard::spawn_exact_current(TEST_NAME, MODE, EVENT_TIMEOUT).unwrap();
    child.wait_for_readiness(READY, EVENT_TIMEOUT).unwrap();
    let child_id = child.id().to_string();
    let signal_status = Command::new("kill")
        .args(["-TERM", child_id.as_str()])
        .status()
        .unwrap();
    assert!(signal_status.success(), "failed to signal runtime child");
    let status = child.wait_bounded(EVENT_TIMEOUT).unwrap();
    assert!(
        status.success(),
        "runtime signal child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(child.stdout()),
        String::from_utf8_lossy(child.stderr())
    );
    assert_eq!(
        String::from_utf8_lossy(child.stdout())
            .matches(MARKER)
            .count(),
        1,
        "runtime child did not report exactly one drained marker"
    );
}
