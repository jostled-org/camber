use crate::common;

use camber::RuntimeError;
use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::sync::Arc;
use std::time::{Duration, Instant};

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);
/// Requests the configured server must hold open at once.
const CONCURRENT_REQUESTS: usize = 3;
/// The drain window the wedged-child case configures and then measures.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(200);

/// The configured server serves its requests concurrently, not one at a time.
///
/// The handlers rendezvous on a barrier before any of them answers, so no
/// response can be produced until all three are in flight together. A server
/// that serialized them would leave the barrier one participant short and the
/// bounded wait below would expire — the claim is in the handler, not in a
/// count of the responses the test itself collected.
#[test]
fn builder_configures_concurrent_requests() {
    runtime::builder()
        .worker_threads(2)
        .keepalive_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let barrier = Arc::new(tokio::sync::Barrier::new(CONCURRENT_REQUESTS));
            let mut router = Router::new();
            router.get("/slow", move |_req: &Request| {
                let handler_barrier = Arc::clone(&barrier);
                async move {
                    handler_barrier.wait().await;
                    Response::text(200, "done")
                }
            });

            let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
            let addr = server.local_addr();
            let url = format!("http://{addr}/slow");

            let responses = common::block_on(async {
                tokio::time::timeout(
                    EVENT_TIMEOUT,
                    futures_util::future::join_all(
                        (0..CONCURRENT_REQUESTS).map(|_| http::get(&url)),
                    ),
                )
                .await
            })
            .expect("the server never held all three requests in flight at once");

            assert_eq!(
                responses.len(),
                CONCURRENT_REQUESTS,
                "the barrier released without one response per request"
            );
            responses.into_iter().for_each(|response| {
                assert_eq!(response.unwrap().status(), 200);
            });

            server.shutdown_bounded(EVENT_TIMEOUT).unwrap();
            runtime::request_shutdown();
        })
        .unwrap();
}

/// The configured shutdown timeout is what escalates a drain the wedged child
/// never lets finish.
#[test]
fn builder_configures_shutdown_timeout() {
    // The child never observes ScopeClosing, so the drain must escalate: the
    // handle and the instant the drain started leave through a shared slot,
    // because a displaced runtime result drops the closure's value.
    let wedged = common::WedgedHandle::new();
    let closure_wedged = wedged.clone();

    let outcome = runtime::builder()
        .shutdown_timeout(DRAIN_TIMEOUT)
        .run(move || {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let task = camber::spawn_async(async move {
                let _ = entered_tx.send(());
                std::future::pending::<()>().await;
            });
            common::block_on(common::join_bounded(entered_rx, EVENT_TIMEOUT)).unwrap();
            // The drain's clock starts at the close transition this call
            // triggers, so it is read here rather than around `run`. An instant
            // taken outside would charge the drain for Tokio runtime
            // construction, the closure body, and executor teardown, and a
            // drain that escalated at 0 ms would still measure over the
            // configured timeout whenever that surrounding work cost as much.
            runtime::request_shutdown();
            closure_wedged.record((task, Instant::now()));
        });

    assert!(
        matches!(outcome, Err(RuntimeError::ScopeDrainTimeout(1))),
        "the safety-net timeout did not report the wedged child: {outcome:?}"
    );

    let (task, drain_started) = wedged.take();
    let elapsed = drain_started.elapsed();

    // Both bounds, because only the pair proves the configured timeout drove
    // the escalation: the upper one alone passes for a drain that gave up
    // instantly, and `ScopeDrainTimeout(1)` reports that escalation happened,
    // not which timeout produced it.
    //
    // No tolerance under the configured timeout. The measurement starts inside
    // the drain window and the timer wheel rounds up rather than firing early,
    // so an escalation genuinely driven by that timeout always measures longer.
    // Slack here would admit the one alternative the lower bound exists to
    // reject: an escalation that fired early. Jitter goes into the upper bound,
    // which carries the whole margin for the reference environment
    // (macOS/Linux CI runners, debug profile, tests running in parallel).
    assert!(
        elapsed >= DRAIN_TIMEOUT && elapsed < Duration::from_secs(1),
        "expected the configured {DRAIN_TIMEOUT:?} shutdown timeout to drive escalation, got {elapsed:?}"
    );

    // Joined off a throwaway Tokio runtime: the Camber runtime that owned this
    // handle has already returned, so nothing here can mint the context whose
    // absence the handle's result depends on.
    let joined = common::block_on_detached(common::join_bounded(task, EVENT_TIMEOUT));
    // The documented forced-abort result, named rather than merely "an error":
    // `Cancelled` belongs to `AsyncJoinHandle::cancel()`, so accepting any `Err`
    // here would pass for an escalation that took the cooperative path instead.
    common::assert_forced_abort(&joined);
}
