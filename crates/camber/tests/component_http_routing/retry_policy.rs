use crate::retry_upstream::{
    Answer, CLIENT_METHODS, PeerEvent, RunnableDriver, SAFE_METHOD_COUNT, ScriptedUpstream,
    UNSAFE_METHODS, assert_released, describe, send_method, settled, spawn_get, within_watchdog,
};
use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::time::Duration;

const GENERATED_RETRY_CASES: u64 = 24;
const RETRY_COUNT_BOUND: NonZeroUsize = NonZeroUsize::new(5).unwrap();

/// The one route every method-eligibility row dispatches against.
const METHOD_RETRY_PATH: &str = "/method-retry";

/// One counter slot and one answered status per method, in
/// [`CLIENT_METHODS`] order, so the safe methods occupy the leading slots.
const METHODS: usize = CLIENT_METHODS.len();

/// The statuses the client's transient-status policy admits for another
/// attempt.
const TRANSIENT_STATUSES: [u16; 4] = [429, 502, 503, 504];

/// Statuses no policy may repeat: one is a refusal the server already decided,
/// the other is the answer itself.
const SETTLED_STATUSES: [u16; 2] = [400, 200];

/// The failure a replayed ambiguous request reports.
const AMBIGUOUS_REPLAY: &str = "ambiguous unsafe request was replayed";

/// The counted route family every method-eligibility row shares.
///
/// One status is armed for the whole row, and one counter slot records each
/// method's dispatches, so an attempt arithmetic is read per method rather than
/// inferred from a total.
#[derive(Clone)]
struct MethodRetryProbe {
    calls: Arc<[AtomicU32; METHODS]>,
    status: Arc<AtomicU16>,
}

impl MethodRetryProbe {
    fn new() -> Self {
        Self {
            calls: Arc::new(std::array::from_fn(|_| AtomicU32::new(0))),
            status: Arc::new(AtomicU16::new(503)),
        }
    }

    /// Count one dispatch of the method at `index` and report the armed status.
    fn record(&self, index: usize) -> u16 {
        self.calls[index].fetch_add(1, Ordering::Relaxed);
        self.status.load(Ordering::Relaxed)
    }

    /// Arm every route with `status` and forget the previous row's counts.
    fn arm(&self, status: u16) {
        self.status.store(status, Ordering::Relaxed);
        self.calls
            .iter()
            .for_each(|count| count.store(0, Ordering::Relaxed));
    }

    fn attempts(&self) -> [u32; METHODS] {
        std::array::from_fn(|index| self.calls[index].load(Ordering::Relaxed))
    }
}

/// Register one counted route per method.
///
/// Every route differs only in the `Router` setter it calls and the counter
/// slot it owns, so the pair is written once and the bodies follow from it.
macro_rules! method_retry_routes {
    ($router:expr, $probe:expr, $($index:expr => $method:ident),+ $(,)?) => {
        $({
            let probe = $probe.clone();
            $router.$method(METHOD_RETRY_PATH, move |_req: &Request| {
                let status = probe.record($index);
                async move { Response::empty(status) }
            });
        })+
    };
}

fn register_method_retry_routes(router: &mut Router, probe: &MethodRetryProbe) {
    method_retry_routes!(
        router,
        probe,
        0 => get,
        1 => head,
        2 => options,
        3 => post,
        4 => put,
        5 => patch,
        6 => delete,
    );
}

/// Compare observed attempts per method, naming the method that disagreed.
fn assert_attempts(attempts: [u32; METHODS], expected: [u32; METHODS], context: &str) {
    let rows = attempts.iter().zip(expected).zip(CLIENT_METHODS);
    for ((observed, wanted), name) in rows {
        assert_eq!(
            *observed, wanted,
            "{context}: {name} ran {observed} attempts, not {wanted}"
        );
    }
}

/// Dispatch every method once through `client` and report the answered
/// statuses in counter-slot order.
async fn dispatch_every_method(client: &http::ClientBuilder, url: &str) -> [u16; METHODS] {
    let mut answered = [0; METHODS];
    for (status, method) in answered.iter_mut().zip(CLIENT_METHODS) {
        *status = send_method(client, method, url)
            .await
            .unwrap_or_else(|error| panic!("{method} to {url} failed: {error:?}"))
            .status();
    }
    answered
}

fn retry_client(retries: u32, opt_in: bool) -> http::ClientBuilder {
    http::client()
        .retries(retries)
        .backoff(Duration::from_millis(1))
        .retry_unsafe_methods(opt_in)
}

/// Arm every route with `status`, dispatch every method once through `client`,
/// and require that status answered and `expected` attempts per method.
async fn assert_armed_dispatch(
    probe: &MethodRetryProbe,
    url: &str,
    status: u16,
    client: &http::ClientBuilder,
    expected: [u32; METHODS],
    context: &str,
) {
    probe.arm(status);
    let answered = dispatch_every_method(client, url).await;
    assert_eq!(answered, [status; METHODS], "{context}: answered statuses");
    assert_attempts(probe.attempts(), expected, context);
}

/// One transient status: safe methods spend their whole budget, unsafe methods
/// spend it only after the explicit policy admits them.
async fn assert_transient_row(probe: &MethodRetryProbe, url: &str, status: u16, retries: u32) {
    let mut safe_only = [retries + 1; METHODS];
    safe_only[SAFE_METHOD_COUNT..].fill(1);

    assert_armed_dispatch(
        probe,
        url,
        status,
        &retry_client(retries, false),
        safe_only,
        &format!("status {status}, no opt-in"),
    )
    .await;
    assert_armed_dispatch(
        probe,
        url,
        status,
        &retry_client(retries, true),
        [retries + 1; METHODS],
        &format!("status {status}, opted in"),
    )
    .await;
}

/// A status outside the transient set: the server already decided, so no
/// policy repeats the request.
async fn assert_settled_row(probe: &MethodRetryProbe, url: &str, status: u16, retries: u32) {
    assert_armed_dispatch(
        probe,
        url,
        status,
        &retry_client(retries, true),
        [1; METHODS],
        &format!("settled status {status}"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_method_retry_requires_explicit_policy() {
    const RETRIES: u32 = 2;

    let probe = MethodRetryProbe::new();
    let mut router = Router::new();
    register_method_retry_routes(&mut router, &probe);
    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let addr = server.local_addr();
    let url = format!("http://{addr}{METHOD_RETRY_PATH}");

    for status in TRANSIENT_STATUSES {
        assert_transient_row(&probe, &url, status, RETRIES).await;
    }
    for status in SETTLED_STATUSES {
        assert_settled_row(&probe, &url, status, RETRIES).await;
    }

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
    crate::http::assert_address_reused(addr, "method retry probe").await;
}

/// One ambiguous-transport row: an upstream reads the request and then closes
/// without answering.
///
/// The peer's progress is unknowable from the client's side, so the request has
/// no evidence permitting a replay — with or without the unsafe-method policy.
async fn assert_ambiguous_transport_is_once(method: &str, opt_in: bool) {
    const RETRIES: u32 = 2;

    // Every attempt the budget allows reads and closes alike, so a replay is
    // counted rather than parked.
    let script = [Answer::Ambiguous; RETRIES as usize + 1];
    let mut upstream = ScriptedUpstream::bind(&script).await;
    let client = retry_client(RETRIES, opt_in);
    let row = format!("{AMBIGUOUS_REPLAY}? {method} (opt_in={opt_in})");

    let result = send_method(&client, method, &upstream.url("/ambiguous-transport")).await;
    // The first attempt's report is consumed here, so a replay's is the one
    // `finish` finds unconsumed.
    let first = upstream.saw(PeerEvent::Started(0)).await;
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), &row).await;

    if let Err(observed) = first {
        panic!("{row}: the upstream never read the request: {observed}");
    }

    let error = match result {
        Err(error) => error,
        Ok(response) => panic!(
            "{row}: an unanswered request returned {}",
            response.status()
        ),
    };
    assert!(
        matches!(error, RuntimeError::Http(_)),
        "{row}: returned {error} instead of the transport failure"
    );
    assert_eq!(starts, 1, "{row}: started {starts} requests");
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_ambiguous_transport_is_not_replayed() {
    for &method in UNSAFE_METHODS {
        for opt_in in [true, false] {
            let row = assert_ambiguous_transport_is_once(method, opt_in);
            let settled = within_watchdog(row).await;
            assert!(
                settled.is_some(),
                "{method} (opt_in={opt_in}) did not settle"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_retry_attempt_arithmetic_is_exact() {
    let calls = Arc::new(AtomicU32::new(0));
    let handler_calls = Arc::clone(&calls);
    let mut router = Router::new();
    router.get("/generated-retry", move |_req: &Request| {
        handler_calls.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let url = format!("http://{}/generated-retry", server.local_addr());
    let generator = crate::deterministic::DeterministicGenerator::stable();

    for index in 0..GENERATED_RETRY_CASES {
        let mut case = generator.case(index);
        let retries = case.bounded(RETRY_COUNT_BOUND) as u32;
        calls.store(0, Ordering::Relaxed);

        let response = retry_client(retries, false).get(&url).await.unwrap();

        assert_eq!(response.status(), 503, "{case}: retries={retries}");
        assert_eq!(
            calls.load(Ordering::Relaxed),
            retries + 1,
            "{case}: retries={retries}"
        );
    }

    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

/// The transient answer's connection closes while the clock is held still, so
/// the release cannot have waited for the delay to end. The delay is stepped
/// over only after the peer has seen that release.
#[tokio::test(start_paused = true)]
async fn transient_response_is_released_before_retry_backoff() {
    const CONTEXT: &str = "transient release before backoff";
    const BACKOFF: Duration = Duration::from_millis(500);

    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::Transient, Answer::Complete]).await;
    let client = http::client().retries(1).backoff(BACKOFF);
    let call = spawn_get(client, upstream.url("/retry-release"));

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Disposed(0)], CONTEXT)
        .await;
    tokio::time::advance(2 * BACKOFF).await;
    upstream.finish_answered(1, call, driver, CONTEXT).await;
}

#[camber::test]
async fn client_retries_on_transient_error() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/retry", move |_req: &Request| {
        let n = c.fetch_add(1, Ordering::Relaxed);
        async move {
            match n < 2 {
                true => Response::empty(503),
                false => Response::text(200, "ok"),
            }
        }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/retry"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");
    assert_eq!(count.load(Ordering::Relaxed), 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_does_not_retry_on_4xx() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/bad", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::text(400, "bad request") }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/bad"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    assert_eq!(count.load(Ordering::Relaxed), 1);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_exhausts_retries_and_returns_last_error() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/fail", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(2)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/fail"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    assert_eq!(count.load(Ordering::Relaxed), 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_free_functions_do_not_retry() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/once", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::get(&format!("http://{addr}/once")).await.unwrap();

    assert_eq!(resp.status(), 503);
    assert_eq!(count.load(Ordering::Relaxed), 1);

    runtime::request_shutdown();
}

/// Each attempt's head is held until the row steps the clock onto that
/// attempt's own lifetime, and the delay between them is stepped over only
/// after the peer has seen the timed-out attempt's transport closed.
#[tokio::test(start_paused = true)]
async fn client_retries_on_timeout() {
    const CONTEXT: &str = "per-attempt timeout retry";
    const ATTEMPT: Duration = Duration::from_millis(50);
    const BACKOFF: Duration = Duration::from_millis(10);

    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallHead, Answer::StallHead]).await;
    let client = http::client()
        .retries(1)
        .backoff(BACKOFF)
        .request_timeout(ATTEMPT);
    let call = spawn_get(client, upstream.url("/slow"));

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], CONTEXT)
        .await;
    tokio::time::advance(ATTEMPT).await;
    upstream.expect(PeerEvent::Released(0), CONTEXT).await;
    tokio::time::advance(2 * BACKOFF).await;
    upstream
        .expect_each(&[PeerEvent::Started(1), PeerEvent::Stalled(1)], CONTEXT)
        .await;
    tokio::time::advance(ATTEMPT).await;
    let result = settled(call).await;
    let released = upstream.saw(PeerEvent::Released(1)).await;
    let starts = upstream.finish(driver, CONTEXT).await;

    assert!(
        matches!(result, Some(Err(RuntimeError::Timeout))),
        "{CONTEXT}: expected Timeout, got {}",
        describe(&result)
    );
    assert_released(released, CONTEXT);
    assert_eq!(starts, 2, "{CONTEXT}: the call started {starts} requests");
}
