//! Task ownership follows the server across client disconnects, stream resets,
//! and executor moves.

use super::fixture::{BOUND, DRIVER_AND_PRODUCER, DRIVER_ONLY, bounded, with_drain_window};
use super::peer::{send, start_request};
use super::request_tasks::{assert_tasks, h2_answer, task_results, task_router};
use super::routes::probe_router;
use crate::common::{PersistentH2Client, observe_armed_sequence};
use camber::http::{Next, Request, Response, Router};
use camber::runtime;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, mpsc};
use tokio::sync::Semaphore;

#[derive(Clone, Copy)]
enum Child {
    Blocking,
    Async,
}

/// Report that Hyper dropped the reset request future.
struct HandlerDrop(mpsc::Sender<()>);

impl Drop for HandlerDrop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Release the child even if an assertion or drain observation fails.
struct ChildRelease(Arc<Semaphore>);

impl Drop for ChildRelease {
    fn drop(&mut self) {
        self.0.add_permits(1);
    }
}

fn spawn_held_child(kind: Child, entered: mpsc::Sender<()>, release: Arc<Semaphore>) {
    match kind {
        Child::Blocking => drop(camber::spawn(move || {
            entered.send(()).unwrap();
            runtime::block_on(async { release.acquire().await.unwrap().forget() });
        })),
        Child::Async => drop(camber::spawn_async(async move {
            entered.send(()).unwrap();
            release.acquire().await.unwrap().forget();
        })),
    }
}

/// Hold the sibling inside its request future until the other stream is reset.
fn gate_sibling(router: &mut Router) -> (mpsc::Receiver<()>, Arc<Semaphore>) {
    let (entered, entry) = mpsc::channel();
    let release = Arc::new(Semaphore::new(0));
    let gate = Arc::clone(&release);
    router.use_middleware(move |request: &Request, next: Next| {
        let watched = request.path() == "/tasks";
        let response = next.call(request);
        let entered = entered.clone();
        let gate = Arc::clone(&gate);
        async move {
            match watched {
                true => {
                    entered.send(()).unwrap();
                    gate.acquire().await.unwrap().forget();
                }
                false => {}
            }
            response.await
        }
    });
    (entry, release)
}

/// What a `/held` route reports back: its child's entry and its own drop.
struct HeldRoute {
    entry: mpsc::Receiver<()>,
    drops: mpsc::Receiver<()>,
    release: ChildRelease,
}

/// Serve `/held`: admit one held child, then park until Hyper drops the
/// request future.
fn route_held_child(router: &mut Router, kind: Child) -> HeldRoute {
    let (entered, entry) = mpsc::channel();
    let (dropped, drops) = mpsc::channel();
    let release = Arc::new(Semaphore::new(0));
    let child_release = Arc::clone(&release);
    router.get("/held", move |_: &Request| {
        spawn_held_child(kind, entered.clone(), Arc::clone(&child_release));
        let guard = HandlerDrop(dropped.clone());
        async move {
            std::future::pending::<()>().await;
            drop(guard);
            Response::text(200, "released")
        }
    });
    HeldRoute {
        entry,
        drops,
        release: ChildRelease(release),
    }
}

fn reset_keeps_child_owned(kind: Child) {
    let mut router = task_router();
    let (sibling_entry, sibling_release) = gate_sibling(&mut router);
    let HeldRoute {
        entry,
        drops,
        release,
    } = route_held_child(&mut router, kind);
    let observed = with_drain_window(
        None,
        router,
        move |addr| {
            bounded("reset request and sibling", async {
                let mut client = PersistentH2Client::connect(addr, BOUND).await;
                let mut reset = client.open_paced("GET", "/held", "localhost", &[]).await;
                reset.finish();
                entry.recv_timeout(BOUND).unwrap();
                let mut sibling = client.open_paced("GET", "/tasks", "localhost", &[]).await;
                sibling.finish();
                sibling_entry.recv_timeout(BOUND).unwrap();
                reset.reset();
                drop(reset);
                drops.recv_timeout(BOUND).unwrap();
                sibling_release.add_permits(1);
                assert_tasks(sibling.answer().await, "Ok(40)/Ok(41)");
                client.close().await;
            });
            release
        },
        DRIVER_AND_PRODUCER,
        |_, release| {
            drop(release);
        },
    );
    assert!(
        observed.probed.is_some(),
        "the reset discarded the child's scope owner"
    );
    assert!(
        observed.reached_zero,
        "the scope never drained after the reset"
    );
}

/// An HTTP/1.1 client leaves after its handler admitted a child. The scope
/// drain must still count that child until it is released.
fn disconnect_keeps_child_owned(kind: Child) {
    let mut router = probe_router();
    let HeldRoute {
        entry,
        drops,
        release,
    } = route_held_child(&mut router, kind);
    let observed = with_drain_window(
        None,
        router,
        move |addr| {
            let client = start_request(addr, "/held");
            entry
                .recv_timeout(BOUND)
                .expect("the handler's child never entered");
            drop(client);
            drops
                .recv_timeout(BOUND)
                .expect("the HTTP/1.1 disconnect never dropped the request future");
            release
        },
        DRIVER_AND_PRODUCER,
        |_, release| {
            drop(release);
        },
    );
    assert!(
        observed.probed.is_some(),
        "the HTTP/1.1 disconnect discarded the child's scope owner"
    );
    assert!(
        observed.reached_zero,
        "the scope never drained after the HTTP/1.1 disconnect"
    );
}

#[test]
fn request_tasks_http1_disconnect_preserves_blocking_child() {
    disconnect_keeps_child_owned(Child::Blocking);
}

#[test]
fn request_tasks_http1_disconnect_preserves_async_child() {
    disconnect_keeps_child_owned(Child::Async);
}

#[test]
fn request_tasks_reset_preserves_blocking_child_and_sibling() {
    reset_keeps_child_owned(Child::Blocking);
}

#[test]
fn request_tasks_reset_preserves_async_child_and_sibling() {
    reset_keeps_child_owned(Child::Async);
}

#[test]
fn request_tasks_capture_owner_a_when_server_is_polled_by_open_owner_b() {
    let (published, arrival) = mpsc::channel();
    let (release, held) = tokio::sync::oneshot::channel::<()>();
    let (outcome, observed) = observe_armed_sequence(
        |builder| builder.worker_threads(4),
        move |gate| {
            let server = bounded("owner A server construction", async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                (
                    addr,
                    camber::http::serve_async(listener, task_router()).unwrap(),
                )
            });
            drop(camber::spawn_async(async move {
                let _ = held.await;
            }));
            published.send(server).unwrap();
            gate.arm(DRIVER_ONLY);
        },
        move |watch| {
            watch.wait_armed_within(BOUND);
            let (addr, server) = arrival.recv_timeout(BOUND).unwrap();
            let observed = watch.probe(DRIVER_ONLY, |_| {
                runtime::builder()
                    .worker_threads(4)
                    .run(|| {
                        bounded("owner B serving owner A", async {
                            assert_eq!(camber::spawn_async(async { 99 }).await.unwrap(), 99);
                            let serving = tokio::spawn(server);
                            assert_tasks(
                                h2_answer(addr).await,
                                "Err(ScopeClosed)/Err(ScopeClosed)",
                            );
                            let response = tokio::task::spawn_blocking(move || {
                                send(addr, "GET", "/tasks", "owner A HTTP/1 tasks")
                            })
                            .await
                            .unwrap();
                            assert_tasks(response, "Err(ScopeClosed)/Err(ScopeClosed)");
                            serving.abort();
                            assert!(serving.await.unwrap_err().is_cancelled());
                            assert_eq!(camber::spawn_async(async { 100 }).await.unwrap(), 100);
                        });
                    })
                    .unwrap();
            });
            drop(release);
            observed.is_some()
        },
    );
    outcome.unwrap();
    assert!(
        observed,
        "owner A's drain window was never observed while owner B served"
    );
}

/// Serve `/gated`, which reports entry and suspends before it runs both task
/// forms, and `/panic`, which unwinds on its second poll.
fn restoration_router(
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: &Arc<Semaphore>,
) -> Router {
    let release = Arc::clone(release);
    let mut router = probe_router();
    router.get("/gated", move |_: &Request| {
        let entered = entered.clone();
        let release = Arc::clone(&release);
        async move {
            entered.send(()).unwrap();
            release.acquire().await.unwrap().forget();
            Response::text(200, &task_results(40, Arc::new(AtomicUsize::new(0))).await)
        }
    });
    router.get("/panic", |_: &Request| async {
        tokio::task::yield_now().await;
        unwind_handler()
    });
    router
}

fn unwind_handler() -> Response {
    panic!("the handler unwinds inside the captured-absence scope")
}

/// Both task forms reach the thread's installed runtime from the test task.
async fn assert_thread_owner_visible(step: &str) {
    assert_eq!(
        task_results(7, Arc::new(AtomicUsize::new(0))).await,
        "Ok(7)/Ok(8)",
        "the thread's runtime was not visible {step}"
    );
}

/// Wait until the gated handler has entered and suspended.
async fn await_gated_entry(entries: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
    tokio::time::timeout(BOUND, entries.recv())
        .await
        .expect("the gated handler never entered")
        .expect("the gated route dropped its entry sender");
}

/// On one thread, a server that captured no runtime polls its handlers between
/// test-task polls that see the thread-local runtime. Every handler poll must
/// see no runtime. Every poll after it, including after an unwind, must see
/// the thread's runtime again.
#[tokio::test]
async fn request_tasks_captured_absence_restores_thread_owner_after_each_poll() {
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server =
        camber::http::serve_async(listener, restoration_router(entered, &release)).unwrap();
    let _owner = camber::runtime_test_support::install_runtime_context();
    assert_thread_owner_visible("before serving").await;
    let serving = tokio::spawn(server);

    let mut client = PersistentH2Client::connect(addr, BOUND).await;
    let mut gated = client.open_paced("GET", "/gated", "localhost", &[]).await;
    gated.finish();
    await_gated_entry(&mut entries).await;
    assert_thread_owner_visible("after a suspended HTTP/2 handler poll").await;
    release.add_permits(1);
    assert_tasks(gated.answer().await, "Err(NoRuntime)/Err(NoRuntime)");
    assert_thread_owner_visible("after the HTTP/2 handler completed").await;

    let http1 = tokio::task::spawn_blocking(move || send(addr, "GET", "/gated", "gated HTTP/1"));
    await_gated_entry(&mut entries).await;
    assert_thread_owner_visible("after a suspended HTTP/1 handler poll").await;
    release.add_permits(1);
    assert_tasks(http1.await.unwrap(), "Err(NoRuntime)/Err(NoRuntime)");
    assert_thread_owner_visible("after the HTTP/1 handler completed").await;

    let mut unwound = client.open_paced("GET", "/panic", "localhost", &[]).await;
    unwound.finish();
    assert!(
        unwound.try_answer().await.is_err(),
        "the panicking HTTP/2 handler produced a response instead of unwinding its stream task"
    );
    assert_thread_owner_visible("after an HTTP/2 handler poll unwound").await;

    drop((gated, unwound));
    client.close().await;
    serving.abort();
    assert!(serving.await.unwrap_err().is_cancelled());
}
