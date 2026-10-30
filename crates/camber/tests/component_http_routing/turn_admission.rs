use crate::http as wire;
use crate::runtime_support;
use camber::http::{Rejection, RejectionContext, RejectionKind, Response, Router, StreamResponse};
use camber::{RuntimeError, Turn, TurnGuard, TurnRefused, runtime};
use std::future::{Future, pending, poll_fn, ready};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// How long a case waits for a handler to report a queue transition.
const QUEUE_BOUND: Duration = Duration::from_secs(5);

fn routes() -> Router {
    let mut router = Router::new();
    let deadline = Turn::new();
    router.get("/deadline", move |_| {
        let turn = deadline.clone();
        async move {
            let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            let guard = turn.acquire_until(Some(expired), pending::<()>()).await?;
            drop(guard);
            Response::empty(200)
        }
    });
    let cancelled = Turn::new();
    router.post_stream("/cancelled", move |_| {
        let turn = cancelled.clone();
        async move {
            let guard = turn.acquire_until(None, ready(())).await?;
            drop(guard);
            Response::empty(200)
        }
    });
    let closed = Turn::new();
    closed.close();
    router.get("/closed", move |_| {
        let turn = closed.clone();
        async move {
            let guard = turn.acquire().await?;
            drop(guard);
            Response::empty(200)
        }
    });
    router
}

#[test]
fn turn_refusals_are_service_unavailable_without_an_invented_retry_after() {
    runtime_support::test_runtime()
        .run(|| {
            let address = runtime_support::spawn_server(routes());
            for (method, path) in [
                ("GET", "/deadline"),
                ("POST", "/cancelled"),
                ("GET", "/closed"),
            ] {
                let response = wire::send(address, method, path, &[], &[]);
                assert_eq!(response.status, 503, "{path}");
                assert_eq!(response.text().as_ref(), "service unavailable", "{path}");
                assert!(response.header("retry-after").is_none(), "{path}");
            }
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn turn_refusals_have_one_typed_mapping_on_buffered_and_streaming_routes() {
    runtime_support::test_runtime()
        .run(|| {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let router = routes().rejection_mapper(
                move |failure: &Rejection, context: &RejectionContext| {
                    assert_eq!(failure.kind(), RejectionKind::TurnAdmission);
                    assert_eq!(failure.status(), 503);
                    assert!(!context.raw_path().is_empty());
                    observed.fetch_add(1, Ordering::SeqCst);
                    Response::text(429, "queue unavailable")
                        .map(|response| response.with_header("Retry-After", "2"))
                },
            );
            let address = runtime_support::spawn_server(router);
            for (method, path) in [
                ("GET", "/deadline"),
                ("POST", "/cancelled"),
                ("GET", "/closed"),
            ] {
                let response = wire::send(address, method, path, &[], &[]);
                assert_eq!(response.status, 429, "{path}");
                assert_eq!(response.header("retry-after"), Some("2"), "{path}");
            }
            assert_eq!(calls.load(Ordering::SeqCst), 3);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn turn_rejection_mapper_can_answer_429_for_an_expired_deadline_only() {
    runtime_support::test_runtime()
        .run(|| {
            let router =
                routes().rejection_mapper(
                    |failure: &Rejection, _: &RejectionContext| match failure.turn_refusal() {
                        Some(TurnRefused::Deadline) => Response::text(429, "queue full")
                            .map(|response| response.with_header("Retry-After", "2")),
                        Some(TurnRefused::Cancelled | TurnRefused::Closed) | None => {
                            Response::text(failure.status(), failure.message())
                        }
                    },
                );
            let address = runtime_support::spawn_server(router);
            for (method, path, status, retry_after) in [
                ("GET", "/deadline", 429, Some("2")),
                ("POST", "/cancelled", 503, None),
                ("GET", "/closed", 503, None),
                ("GET", "/missing", 404, None),
            ] {
                let response = wire::send(address, method, path, &[], &[]);
                assert_eq!(response.status, status, "{path}");
                assert_eq!(response.header("retry-after"), retry_after, "{path}");
            }
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn turn_rejection_names_its_refusal_cause() {
    let plain = Rejection::new(RejectionKind::TurnAdmission, 503, "service unavailable").unwrap();
    assert_eq!(plain.turn_refusal(), None);
    for refusal in [
        TurnRefused::Deadline,
        TurnRefused::Cancelled,
        TurnRefused::Closed,
    ] {
        let named = plain.clone().with_turn_refusal(refusal);
        assert_eq!(named.turn_refusal(), Some(refusal));
        assert_eq!(named.kind(), RejectionKind::TurnAdmission);
        assert_eq!(named.status(), 503);
    }
}

/// Acquire a turn, and report once the acquisition holds a queue entry.
///
/// The report is sent after the first poll returned `Pending`, so a case that
/// receives it reads `queued()` after registration rather than racing it.
async fn acquire_announced<C: Future>(
    turn: &Turn,
    cancel: C,
    registered: &UnboundedSender<()>,
) -> Result<TurnGuard, RuntimeError> {
    let acquisition = turn.acquire_until(None, cancel);
    tokio::pin!(acquisition);
    match poll_fn(|context| Poll::Ready(acquisition.as_mut().poll(context))).await {
        Poll::Ready(outcome) => outcome,
        Poll::Pending => {
            registered.send(()).unwrap();
            acquisition.await
        }
    }
}

/// Wait for one handler report, inside the case's bound.
fn await_report<T>(reports: &mut UnboundedReceiver<T>, what: &str) -> T {
    runtime_support::block_on(tokio::time::timeout(QUEUE_BOUND, reports.recv()))
        .unwrap_or_else(|_| panic!("{what} was not reported in time"))
        .unwrap_or_else(|| panic!("the handler dropped its {what} report"))
}

/// Reports when the handler future that owns it is released.
///
/// A peer disconnect drops the handler future rather than waking it, so the
/// release is the one point a case can wait on.
struct ReleaseReport(UnboundedSender<()>);

impl Drop for ReleaseReport {
    fn drop(&mut self) {
        self.0.send(()).unwrap_or_default();
    }
}

fn disconnect_service(
    turn: Turn,
    registered: UnboundedSender<()>,
    released: UnboundedSender<()>,
) -> Router {
    let mut router = Router::new();
    router.post_stream("/queued", move |request| {
        let turn = turn.clone();
        let registered = registered.clone();
        let released = released.clone();
        let disconnected = request.on_disconnect();
        async move {
            // Bound in the outer block, so the acquisition in the statement
            // below is always dropped first, with its queue entry.
            let release = ReleaseReport(released);
            let guard = acquire_announced(&turn, disconnected.cancelled(), &registered).await?;
            drop((guard, release));
            Response::empty(200)
        }
    });
    router
}

#[test]
fn turn_queued_request_leaves_the_queue_when_its_client_disconnects() {
    runtime_support::test_runtime()
        .run(|| {
            let turn = Turn::new();
            let (registered, mut registrations) = unbounded_channel();
            let (released, mut releases) = unbounded_channel();
            let address = runtime_support::spawn_server(disconnect_service(
                turn.clone(),
                registered,
                released,
            ));
            let held = runtime_support::block_on(turn.acquire()).unwrap();
            let mut client = TcpStream::connect_timeout(&address, QUEUE_BOUND).unwrap();
            wire::write_request(&mut client, "POST", "/queued", &[], &[]).unwrap();
            await_report(&mut registrations, "queue registration");
            assert_eq!(turn.queued(), 1);
            drop(client);
            await_report(&mut releases, "handler release");
            assert_eq!(turn.queued(), 0, "the disconnected waiter left the queue");
            assert_eq!(
                format!("{held:?}"),
                "TurnGuard { permits: 1 }",
                "the holder keeps its turn"
            );
            drop(held);
            drop(runtime_support::block_on(turn.acquire()).unwrap());
            runtime::request_shutdown();
        })
        .unwrap();
}

fn queued_service(turn: Turn, registered: UnboundedSender<()>) -> Router {
    let mut router = Router::new();
    router.post_stream("/work", move |request| {
        let turn = turn.clone();
        let registered = registered.clone();
        let streaming = request.body() == "stream";
        let disconnected = request.on_disconnect();
        async move {
            let guard = acquire_announced(&turn, disconnected.cancelled(), &registered).await?;
            let response = match streaming {
                false => Response::json(200, &serde_json::json!({"ready": true}))?.into(),
                true => {
                    let (response, writer) = StreamResponse::sse();
                    writer.data("ready").await?;
                    response
                }
            };
            drop(guard);
            Ok::<StreamResponse, RuntimeError>(response)
        }
    });
    router
}

#[test]
fn turn_queued_service_delivers_json_and_sse_after_resource_release() {
    runtime_support::test_runtime()
        .run(|| {
            let turn = Turn::new();
            let (registered, mut registrations) = unbounded_channel();
            let address = runtime_support::spawn_server(queued_service(turn.clone(), registered));
            for (body, content_type, expected) in [
                ("json", "application/json", "{\"ready\":true}"),
                ("stream", "text/event-stream", "data: ready\n\n"),
            ] {
                let held = runtime_support::block_on(turn.acquire()).unwrap();
                let request = camber::spawn(move || {
                    wire::send(address, "POST", "/work", &[], body.as_bytes())
                });
                await_report(&mut registrations, "queue registration");
                assert_eq!(turn.queued(), 1);
                drop(held);
                let response = request.join().unwrap();
                assert_eq!(response.status, 200);
                assert_eq!(response.header("content-type"), Some(content_type));
                assert_eq!(response.text().as_ref(), expected);
                assert_eq!(turn.queued(), 0);
                drop(runtime_support::block_on(turn.acquire()).unwrap());
            }
            runtime::request_shutdown();
        })
        .unwrap();
}
