//! Application body-admission policy, entered through a live buffered route.
//!
//! What each case asserts is the answer a peer was given and what the policy,
//! the mapper, and the handler were — or were not — asked to do for it. The
//! lifecycle observer contributes counters only.

use crate::http as wire;
use crate::rejection_support::{
    Collapsed, Journal, Observed, REDACTED_BODY, assert_classification, assert_no_private_text,
    assert_request_id_shape, collapsing_mapper, counting_echo, only, recording_mapper,
};
use crate::runtime_support as common;

use camber::http::mock::ScopedRequestBodyOwner;
use camber::http::{BodyAdmission, BodyAdmissionContext, RejectionKind, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The private text an admission error carries, and no peer may see.
const POLICY_SECRET: &str = "policy-private-diagnostic";
/// The private text an admission panic carries, and no peer may see.
const PANIC_SECRET: &str = "policy-panic-payload";
/// The status the collapsing mapper answers every category with.
const MAPPED_STATUS: u16 = 429;
/// How long a case waits for its paused handler to report entry.
const HANDSHAKE_BOUND: Duration = Duration::from_secs(5);

/// The route every case refuses on, and the one every case admits on.
const REFUSED_ROUTE: &str = "/refused/:id";
const CONTROL_ROUTE: &str = "/control/:id";

/// A permit whose release is the only thing it records.
///
/// One probe for every section below, so a release a case claims is the permit's
/// own report rather than the listener's count of it.
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// A policy that fails one route and admits the other.
fn failing_policy(
    calls: &Arc<AtomicUsize>,
    fail: fn() -> RuntimeError,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(calls);
    move |context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        match context.route() == REFUSED_ROUTE {
            true => Err(fail()),
            false => Ok(BodyAdmission::new(4096)),
        }
    }
}

/// Register the refused route and the admitted control on one router.
fn admission_routes(router: &mut Router, handled: &Arc<AtomicUsize>) {
    router.post(REFUSED_ROUTE, counting_echo(handled));
    router.post(CONTROL_ROUTE, counting_echo(handled));
}

/// Send one chunked body and read the answer.
///
/// The connection [`wire::send_chunked`] hands back is released here: every row
/// that drives one reads the answer alone, and what a refused connection then
/// does is the acceptance root's claim rather than this one's.
fn chunked_answer(addr: SocketAddr, path: &str, chunks: &[&[u8]]) -> wire::HttpResponse {
    wire::send_chunked(
        addr,
        wire::CLOSE_AFTER_RESPONSE,
        "POST",
        path,
        wire::DEFAULT_HOST,
        chunks,
    )
    .expect("the chunked body was answered")
    .0
}

/// Send one body that stops being readable and read the answer.
///
/// [`chunked_answer`] for the framing that breaks mid-body, and the same
/// release.
fn unreadable_answer(addr: SocketAddr, path: &str) -> wire::HttpResponse {
    wire::send_unreadable_body(addr, wire::CLOSE_AFTER_RESPONSE, "POST", path, "text/plain")
        .expect("the truncated body was answered")
        .0
}

/// The diagnostic-bearing failure the admission policy returns.
///
/// One producer for both halves of the case, so the built-in default and the
/// custom mapper are answering the same error rather than two errors that could
/// drift apart.
fn admission_failure() -> RuntimeError {
    RuntimeError::InvalidArgument(POLICY_SECRET.into())
}

/// Assert the built-in default answers an admission refusal and leaks nothing.
///
/// Its own server, because the point of the row is the answer a router with no
/// configured mapper gives.
fn assert_default_admission_refusal(handled: &Arc<AtomicUsize>, calls: &Arc<AtomicUsize>) {
    let mut defaulted = Router::new();
    admission_routes(&mut defaulted, handled);
    let defaulted = defaulted.body_admission(failing_policy(calls, admission_failure));
    let addr = common::spawn_server(defaulted);

    let by_default = wire::send(addr, "POST", "/refused/1", &[], b"payload");
    assert_eq!(
        by_default.status, 503,
        "an admission refusal defaults to service unavailable"
    );
    assert_eq!(by_default.text().as_ref(), "service unavailable");
    assert_no_private_text(&by_default, &[POLICY_SECRET], "default admission refusal");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the refusal is this policy's own answer, asked for exactly once"
    );
}

/// Assert what one admission refusal's mapper was handed.
///
/// The classification, the identity classification selected, and the absence of
/// the policy's own diagnostic: the mapper sees the category and the route, and
/// never the reason.
fn assert_refusal_mapping(seen: &Observed) {
    assert_classification(
        seen,
        &Collapsed {
            kind: RejectionKind::BodyAdmission,
            status: 503,
            message: "service unavailable",
        },
        "admission refusal",
    );
    assert_eq!(seen.method.as_ref(), "POST");
    assert_eq!(seen.raw_path.as_ref(), "/refused/2");
    assert_eq!(
        seen.route.as_deref(),
        Some(REFUSED_ROUTE),
        "the refusal names the route classification selected"
    );
    assert_request_id_shape(Some(&seen.request_id), "admission refusal");
    assert!(
        !seen.message.contains(POLICY_SECRET),
        "the mapper is given no diagnostic: {seen:?}"
    );
}

#[test]
fn policy_error_maps_body_admission_without_polling_or_handler_entry() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));

            assert_default_admission_refusal(&handled, &calls);

            let port = wire::reserve_request_body_owner();
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let mut mapping = Router::new();
            admission_routes(&mut mapping, &handled);
            let mapping = mapping
                .body_admission(failing_policy(&calls, admission_failure))
                .rejection_mapper(collapsing_mapper(&mapped, "router", MAPPED_STATUS));
            let server = port.serve(mapping);
            let addr = server.addr();

            let refused = wire::send(addr, "POST", "/refused/2", &[], b"payload");
            assert_eq!(
                refused.status, MAPPED_STATUS,
                "a custom mapper answers the admission refusal"
            );
            assert_no_private_text(&refused, &[POLICY_SECRET], "mapped admission refusal");
            assert_refusal_mapping(&only(&mapped, "admission refusal"));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "the mapped refusal is the same policy's answer, asked for once more"
            );

            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "a refused request never reaches the handler"
            );
            // One snapshot for both counters, so the two sentences describe the
            // same moment rather than two.
            let body = server.controller().observed();
            assert_eq!(
                body.frames_polled, 0,
                "a refused request polls no body frame"
            );
            assert_eq!(
                body.permit_owners_dropped, 0,
                "a policy that never returned a permit created no owner"
            );

            let control = wire::send(addr, "POST", "/control/3", &[], b"control-body");
            assert_eq!(control.status, 200);
            assert_eq!(control.body.as_ref(), b"control-body");
            assert_eq!(handled.load(Ordering::SeqCst), 1);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "the admitted control asked the same policy once, and it admitted"
            );
            assert!(
                server.controller().observed().frames_polled > 0,
                "the same counter moves once an admitted body is collected"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

/// A policy that panics on the refused route and admits the other.
fn panicking_policy(
    calls: &Arc<AtomicUsize>,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(calls);
    move |context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            context.route() != REFUSED_ROUTE,
            "{PANIC_SECRET}: the policy declined to answer"
        );
        Ok(BodyAdmission::new(4096))
    }
}

#[test]
fn policy_panic_maps_redacted_internal_service_without_unwind() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));

            let port = wire::reserve_request_body_owner();
            let mut router = Router::new();
            admission_routes(&mut router, &handled);
            let router = router
                .body_admission(panicking_policy(&calls))
                .rejection_mapper(recording_mapper(&mapped, "router"));
            let server = port.serve(router);
            let addr = server.addr();

            let refused = wire::send(addr, "POST", "/refused/1", &[], b"payload");
            assert_eq!(refused.status, 500, "a policy panic answers a redacted 500");
            assert_eq!(refused.text().as_ref(), REDACTED_BODY);
            assert_no_private_text(&refused, &[PANIC_SECRET], "policy panic");

            let seen = only(&mapped, "policy panic");
            assert_classification(
                &seen,
                &Collapsed {
                    kind: RejectionKind::InternalService,
                    status: 500,
                    message: REDACTED_BODY,
                },
                "policy panic",
            );
            assert_eq!(seen.method.as_ref(), "POST");
            assert_eq!(seen.raw_path.as_ref(), "/refused/1");
            assert_eq!(seen.route.as_deref(), Some(REFUSED_ROUTE));
            assert_request_id_shape(Some(&seen.request_id), "policy panic");
            assert!(
                !seen.message.contains(PANIC_SECRET),
                "the panic payload reaches no mapper state: {seen:?}"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "a panicking policy is not re-entered"
            );
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "a panicking policy never reaches the handler"
            );
            assert_eq!(server.controller().observed().frames_polled, 0);
            assert_eq!(server.controller().observed().permit_owners_dropped, 0);

            let control = wire::send(addr, "POST", "/control/2", &[], b"still-serving");
            assert_eq!(
                control.status, 200,
                "the request future did not unwind, so the server still serves"
            );
            assert_eq!(control.body.as_ref(), b"still-serving");
            assert!(server.controller().observed().frames_polled > 0);

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Declared limits, refused before a frame is collected ─────────────

/// The router ceiling every declaration row is measured against.
const DECLARED_CEILING: usize = 64;
/// The narrower maximum the policy selects for its own route.
const SELECTED_MAX: usize = 16;
/// The ceiling Camber clamps every configured value to.
const HARD_CEILING: usize = 256 * 1024 * 1024;

/// The route whose ceiling is the router's own.
const PLAIN_ROUTE: &str = "/declared/:id";
/// The route whose policy selects a narrower maximum and hands over a permit.
const SELECTED_ROUTE: &str = "/selected/:id";
/// The route whose policy admits nothing but an empty body.
const ZERO_ROUTE: &str = "/zero/:id";

/// What one listener and its counters had recorded before a row ran.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Counts {
    policy: usize,
    handled: usize,
    frames: usize,
    permits: usize,
    peak_retained: usize,
}

impl Counts {
    /// Read every counter one row is judged on, at one moment.
    fn read(
        server: &wire::ObservedServer<ScopedRequestBodyOwner>,
        policy: &Arc<AtomicUsize>,
        handled: &Arc<AtomicUsize>,
    ) -> Self {
        let body = server.controller().observed();
        Self {
            policy: policy.load(Ordering::SeqCst),
            handled: handled.load(Ordering::SeqCst),
            frames: body.frames_polled,
            permits: body.permit_owners_dropped,
            peak_retained: body.peak_retained_bytes,
        }
    }

    /// What this row moved, as counts of its own.
    fn since(self, before: Self) -> Self {
        Self {
            policy: self.policy - before.policy,
            handled: self.handled - before.handled,
            frames: self.frames - before.frames,
            permits: self.permits - before.permits,
            peak_retained: self.peak_retained,
        }
    }
}

/// A policy that selects a different maximum for each route it is asked about.
fn selecting_policy(
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(calls);
    let drops = Arc::clone(drops);
    move |context: &BodyAdmissionContext<'_>| {
        calls.fetch_add(1, Ordering::SeqCst);
        match context.route() {
            SELECTED_ROUTE => Ok(BodyAdmission::with_permit(
                SELECTED_MAX,
                DropProbe(Arc::clone(&drops)),
            )),
            ZERO_ROUTE => Ok(BodyAdmission::new(0)),
            _ => Ok(BodyAdmission::new(usize::MAX)),
        }
    }
}

/// Serve the three declaration routes under one configured ceiling.
fn declaration_server(
    handled: &Arc<AtomicUsize>,
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
) -> wire::ObservedServer<ScopedRequestBodyOwner> {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.post(PLAIN_ROUTE, counting_echo(handled));
    router.post(SELECTED_ROUTE, counting_echo(handled));
    router.post(ZERO_ROUTE, counting_echo(handled));
    port.serve(
        router
            .max_request_body(DECLARED_CEILING)
            .body_admission(selecting_policy(calls, drops)),
    )
}

/// Assert the configured ceiling refuses a declaration before the policy is
/// asked, and the route-selected maximum refuses one after it.
fn assert_declaration_refusals(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    calls: &Arc<AtomicUsize>,
    handled: &Arc<AtomicUsize>,
) {
    let before = Counts::read(server, calls, handled);
    let over_configured = wire::send(
        server.addr(),
        "POST",
        "/declared/1",
        &[],
        &[b'x'; DECLARED_CEILING + 1],
    );
    assert_eq!(
        over_configured.status, 413,
        "a declaration above the configured ceiling is refused"
    );
    let configured = Counts::read(server, calls, handled).since(before);
    assert_eq!(
        configured.policy, 0,
        "the configured ceiling refuses before the policy is asked"
    );
    assert_eq!(configured.frames, 0, "a declared refusal polls no frame");
    assert_eq!(
        configured.handled, 0,
        "a declared refusal reaches no handler"
    );
    assert_eq!(
        configured.permits, 0,
        "a refusal before the policy creates no permit owner"
    );

    let before = Counts::read(server, calls, handled);
    let over_selected = wire::send(server.addr(), "POST", "/selected/2", &[], &[b'x'; 32]);
    assert_eq!(
        over_selected.status, 413,
        "a declaration above the selected maximum is refused"
    );
    let selected = Counts::read(server, calls, handled).since(before);
    assert_eq!(
        selected.policy, 1,
        "the selected maximum is refused after exactly one policy call"
    );
    assert_eq!(
        selected.frames, 0,
        "a selected-limit refusal polls no frame"
    );
    assert_eq!(
        selected.handled, 0,
        "a selected-limit refusal reaches no handler"
    );
    assert_eq!(
        selected.permits, 1,
        "the permit the policy handed over is released once"
    );
}

/// Assert a zero maximum refuses the first stated byte and admits an empty body.
fn assert_zero_maximum_rows(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    calls: &Arc<AtomicUsize>,
    handled: &Arc<AtomicUsize>,
) {
    let before = Counts::read(server, calls, handled);
    let one_byte = wire::send(server.addr(), "POST", "/zero/3", &[], b"x");
    assert_eq!(
        one_byte.status, 413,
        "a zero maximum refuses the first stated byte"
    );
    let refused = Counts::read(server, calls, handled).since(before);
    assert_eq!(refused.policy, 1);
    assert_eq!(refused.frames, 0);
    assert_eq!(refused.handled, 0);
    assert_eq!(
        refused.permits, 0,
        "a policy that returned no permit created no owner"
    );

    let before = Counts::read(server, calls, handled);
    let empty = wire::send(server.addr(), "POST", "/zero/4", &[], b"");
    assert_eq!(empty.status, 200, "a zero maximum admits an empty body");
    assert_eq!(
        empty.body.as_ref(),
        b"",
        "the handler echoes the empty body it was given"
    );
    let admitted = Counts::read(server, calls, handled).since(before);
    assert_eq!(admitted.policy, 1);
    assert_eq!(
        admitted.frames, 0,
        "an empty body has no data frame to poll"
    );
    assert_eq!(
        admitted.handled, 1,
        "an admitted empty body reaches the handler"
    );
}

/// Assert declarations at and below the ceiling are collected in full.
fn assert_admitted_declarations(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    calls: &Arc<AtomicUsize>,
    handled: &Arc<AtomicUsize>,
) {
    let before = Counts::read(server, calls, handled);
    let equal = wire::send(
        server.addr(),
        "POST",
        "/declared/5",
        &[],
        &[b'e'; DECLARED_CEILING],
    );
    assert_eq!(
        equal.status, 200,
        "a declaration equal to the ceiling is admitted"
    );
    assert_eq!(
        equal.body.as_ref(),
        &[b'e'; DECLARED_CEILING],
        "the handler echoes the complete body"
    );
    let filled = Counts::read(server, calls, handled).since(before);
    assert_eq!(filled.handled, 1);
    assert!(filled.frames > 0, "an admitted body's frames are polled");

    let before = Counts::read(server, calls, handled);
    let below = wire::send(server.addr(), "POST", "/declared/6", &[], b"below");
    assert_eq!(
        below.status, 200,
        "a declaration below the ceiling is admitted"
    );
    assert_eq!(below.body.as_ref(), b"below");
    let short = Counts::read(server, calls, handled).since(before);
    assert_eq!(short.handled, 1);
    assert!(short.frames > 0);
}

/// Assert the hard ceiling refuses a declaration a configured `usize::MAX`
/// would otherwise have admitted.
///
/// Its own server, because the claim is what a router that configured no
/// reachable ceiling still refuses. The declaration is withheld: a body of that
/// size is not something a test sends, and the refusal must not need it.
fn assert_hard_ceiling_refusal(
    handled: &Arc<AtomicUsize>,
    calls: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
) {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.post(PLAIN_ROUTE, counting_echo(handled));
    let server = port.serve(
        router
            .max_request_body(usize::MAX)
            .body_admission(selecting_policy(calls, drops)),
    );

    let before = Counts::read(&server, calls, handled);
    let (refused, _peer) = wire::send_withheld_declaration(
        server.addr(),
        wire::CLOSE_AFTER_RESPONSE,
        "POST",
        "/declared/7",
        &(HARD_CEILING + 1).to_string(),
    )
    .expect("the withheld oversize declaration was answered");
    assert_eq!(
        refused.status, 413,
        "the hard ceiling refuses what a configured usize::MAX cannot raise"
    );
    let moved = Counts::read(&server, calls, handled).since(before);
    assert_eq!(
        moved.policy, 0,
        "the clamped ceiling refuses before the policy is asked"
    );
    assert_eq!(moved.frames, 0);
    assert_eq!(moved.handled, 0);
    assert_eq!(moved.permits, 0);
}

// ── Every buffered terminal releases one owner ───────────────────────

/// The maximum the terminal matrix's policy selects.
///
/// Equal to the length a stalled request declares, so the deadline row is
/// admitted rather than refused on its declaration before it can stall.
const TERMINAL_MAX: usize = wire::STALLED_CONTENT_LENGTH;
/// The configured ceiling the terminal matrix runs under.
const TERMINAL_CEILING: usize = 1024;
/// How long a release has to settle after the peer has its answer.
const RELEASE_BOUND: Duration = Duration::from_secs(5);
/// How long the deadline row waits for the collection deadline to answer.
///
/// Past the fixed 30-second deadline it is waiting on, so this read never
/// displaces the refusal it exists to observe.
const DEADLINE_ANSWER: Duration = Duration::from_secs(90);

/// Assert the release count settles at exactly `expected`.
///
/// Polled rather than read once: a successful request's owner is released when
/// the request future finishes, which can happen after the peer already has its
/// answer. The bound is the settling budget, not a sleep — a count that is
/// already right returns on the first look.
fn assert_released(drops: &Arc<AtomicUsize>, expected: usize, label: &str) {
    let settled = wire::poll_until(RELEASE_BOUND, || drops.load(Ordering::Acquire) == expected);
    assert!(
        settled,
        "{label}: expected {expected} releases, saw {}",
        drops.load(Ordering::Acquire)
    );
}

/// Register every terminal the matrix drives one admitted request into.
fn terminal_routes(router: &mut Router, handled: &Arc<AtomicUsize>) {
    router.post("/ok/:id", counting_echo(handled));
    router.post("/gated/:id", counting_echo(handled));
    router.post("/refuse/:id", counting_echo(handled));
    router.post("/fail/:id", |_request: &Request| {
        std::future::ready(Err::<Response, RuntimeError>(RuntimeError::BadRequest(
            "the handler declined".into(),
        )))
    });
}

/// Serve the terminal matrix: one permit per admitted request, one refusal.
fn terminal_server(
    handled: &Arc<AtomicUsize>,
    drops: &Arc<AtomicUsize>,
) -> wire::ObservedServer<ScopedRequestBodyOwner> {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    terminal_routes(&mut router, handled);
    router.use_middleware(|request: &Request, next: camber::http::Next| {
        let refuse = request.path().starts_with("/gated/");
        let entered = next.call(request);
        Box::pin(async move {
            match refuse {
                true => Response::text(403, "the gate refused").expect("valid gate status"),
                false => entered.await,
            }
        }) as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    let drops = Arc::clone(drops);
    port.serve(router.max_request_body(TERMINAL_CEILING).body_admission(
        move |context: &BodyAdmissionContext<'_>| match context.raw_path().starts_with("/refuse/") {
            true => Err(RuntimeError::ScopeClosed),
            false => Ok(BodyAdmission::with_permit(
                TERMINAL_MAX,
                DropProbe(Arc::clone(&drops)),
            )),
        },
    ))
}

/// Drive the terminals that complete: empty, collected, gated, and failed.
fn assert_completing_terminals(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    drops: &Arc<AtomicUsize>,
) -> usize {
    let empty = wire::send(server.addr(), "POST", "/ok/1", &[], b"");
    assert_eq!(empty.status, 200);
    assert_eq!(empty.body.as_ref(), b"");
    assert_released(drops, 1, "empty collection and handler completion");

    let collected = wire::send(server.addr(), "POST", "/ok/2", &[], b"collected");
    assert_eq!(collected.status, 200);
    assert_eq!(collected.body.as_ref(), b"collected");
    assert_released(drops, 2, "handler completion");

    let gated = wire::send(server.addr(), "POST", "/gated/3", &[], b"gated");
    assert_eq!(
        gated.status, 403,
        "the gate answered instead of the handler"
    );
    assert_released(drops, 3, "gate refusal after admission");

    let failed = wire::send(server.addr(), "POST", "/fail/4", &[], b"failing");
    assert_eq!(failed.status, 400, "the handler's own failure is mapped");
    assert_released(drops, 4, "handler error");
    4
}

/// Drive the terminals that refuse the body itself.
fn assert_body_refusal_terminals(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    drops: &Arc<AtomicUsize>,
    released: usize,
) -> usize {
    let over = wire::send(
        server.addr(),
        "POST",
        "/ok/5",
        &[],
        &[b'o'; TERMINAL_MAX + 1],
    );
    assert_eq!(
        over.status, 413,
        "a declaration above the selected maximum is refused"
    );
    assert_released(drops, released + 1, "selected-limit refusal");

    let crossing = [b'c'; TERMINAL_MAX];
    let crossed = chunked_answer(server.addr(), "/ok/6", &[&crossing, b"past"]);
    assert_eq!(crossed.status, 413, "the crossing frame is refused");
    assert_released(drops, released + 2, "observed crossing");

    let unreadable = unreadable_answer(server.addr(), "/ok/7");
    assert_eq!(
        unreadable.status, 400,
        "a body that stopped arriving is not a size complaint"
    );
    assert_released(drops, released + 3, "unreadable body");
    released + 3
}

/// Drive the deadline terminal: a body that is promised and never finishes.
fn assert_deadline_terminal(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    drops: &Arc<AtomicUsize>,
    released: usize,
) -> usize {
    let mut peer = wire::connect(server.addr()).expect("the stalled peer connected");
    wire::write_stalled_body(&mut peer, Some(wire::CLOSE_AFTER_RESPONSE), "POST", "/ok/8")
        .expect("the stalled head reached the server");
    let answered = wire::with_read_deadline(&mut peer, DEADLINE_ANSWER, |peer, deadline| {
        wire::read_http_response(peer, Some(deadline))
    })
    .expect("the collection deadline answered");
    assert_eq!(
        answered.status, 408,
        "the collection deadline refuses the unfinished body"
    );
    assert_released(drops, released + 1, "collection deadline");
    released + 1
}

/// Drive the refusal that creates no owner at all.
fn assert_admission_refusal_terminal(
    server: &wire::ObservedServer<ScopedRequestBodyOwner>,
    drops: &Arc<AtomicUsize>,
    released: usize,
) {
    let refused = wire::send(server.addr(), "POST", "/refuse/9", &[], b"refused");
    assert_eq!(
        refused.status, 503,
        "the policy's own refusal is the admission rejection"
    );
    assert_released(
        drops,
        released,
        "a refused admission creates no owner to release",
    );
}

/// Drive the terminal that answers nothing: a handler that unwinds while it owns
/// its admitted request.
///
/// Its own server, on the synchronous serve path, because a handler panic ends
/// its connection task — a fixture that asserts a clean owned-task join would be
/// reporting that unwind rather than this row's claim. The release is read off
/// the permit itself, which is what has to hold whether or not anything else
/// survived.
fn assert_handler_panic_releases_permit() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new();
    router.post("/panic/:id", |_request: &Request| async {
        #[allow(unreachable_code)]
        {
            panic!("the handler panicked while owning its admitted request");
            Response::empty(500)
        }
    });
    let probe = Arc::clone(&drops);
    let router = router.max_request_body(TERMINAL_CEILING).body_admission(
        move |_context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                TERMINAL_MAX,
                DropProbe(Arc::clone(&probe)),
            ))
        },
    );
    let addr = common::spawn_server(router);

    let panicked = wire::request(addr, "POST", "/panic/1", &[], b"panicking", RELEASE_BOUND);
    assert!(
        panicked.is_err(),
        "a panicking handler ends the connection instead of answering: {:?}",
        panicked.map(|answer| answer.status)
    );
    assert_released(&drops, 1, "handler panic while owning the request");
}

#[test]
fn buffered_permit_terminal_matrix_releases_once() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let server = terminal_server(&handled, &drops);

            let released = assert_completing_terminals(&server, &drops);
            let released = assert_body_refusal_terminals(&server, &drops, released);
            let released = assert_deadline_terminal(&server, &drops, released);
            assert_admission_refusal_terminal(&server, &drops, released);
            assert_eq!(
                server.controller().observed().permit_owners_dropped,
                drops.load(Ordering::Acquire),
                "the listener counted the same releases the permits themselves recorded"
            );

            assert_handler_panic_releases_permit();
            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Observed accounting, measured before the frame is kept ───────────

/// The route every observed-accounting row is sent to.
const ACCOUNTED_ROUTE: &str = "/accounted";

/// Serve one echoing route under an exact ceiling, with no policy at all.
///
/// No policy, because what these rows turn on is the bound itself: a configured
/// ceiling reaches the collector as the effective limit, and an application
/// decision beside it would only be a second number to attribute the answer to.
fn accounting_server(
    handled: &Arc<AtomicUsize>,
    ceiling: usize,
) -> wire::ObservedServer<ScopedRequestBodyOwner> {
    let port = wire::reserve_request_body_owner();
    let mut router = Router::new();
    router.post(ACCOUNTED_ROUTE, counting_echo(handled));
    port.serve(router.max_request_body(ceiling))
}

/// Assert bodies below and exactly at the bound are retained whole.
fn assert_retained_within_bound(handled: &Arc<AtomicUsize>) {
    let server = accounting_server(handled, DECLARED_CEILING);
    let below = chunked_answer(server.addr(), ACCOUNTED_ROUTE, &[b"abc", b"defg"]);
    assert_eq!(below.status, 200);
    assert_eq!(
        below.body.as_ref(),
        b"abcdefg",
        "every frame below the bound is retained"
    );
    let within = server.controller().observed();
    assert_eq!(within.frames_polled, 2, "both data frames were polled");
    assert_eq!(
        within.peak_retained_bytes, 7,
        "peak retention is exactly what the request retained"
    );

    let filling = [b'f'; DECLARED_CEILING - 7];
    let equal = chunked_answer(server.addr(), ACCOUNTED_ROUTE, &[b"abcdefg", &filling]);
    assert_eq!(equal.status, 200);
    assert_eq!(
        equal.body.len(),
        DECLARED_CEILING,
        "a body exactly at the bound is retained whole"
    );
    assert_eq!(
        server.controller().observed().peak_retained_bytes,
        DECLARED_CEILING,
        "peak retention reaches the bound and stops there"
    );
}

/// Assert the frame that would cross the bound is measured, refused, and never
/// appended.
fn assert_crossing_frame_is_dropped(handled: &Arc<AtomicUsize>) {
    let server = accounting_server(handled, DECLARED_CEILING);
    let before = handled.load(Ordering::SeqCst);
    let under = [b'u'; DECLARED_CEILING - 4];
    let crossed = chunked_answer(server.addr(), ACCOUNTED_ROUTE, &[&under, b"crossing"]);
    assert_eq!(
        crossed.status, 413,
        "the crossing frame produces the limit refusal"
    );
    let accounted = server.controller().observed();
    assert_eq!(
        accounted.frames_polled, 2,
        "the crossing frame was polled, and measured, before it could be kept"
    );
    assert_eq!(
        accounted.peak_retained_bytes,
        DECLARED_CEILING - 4,
        "accounting runs before the append, so the crossing frame is never retained"
    );
    assert_eq!(
        handled.load(Ordering::SeqCst),
        before,
        "a crossing body reaches no handler"
    );
}

/// Assert a zero bound admits an empty body and refuses the first data frame.
fn assert_zero_bound_rows(handled: &Arc<AtomicUsize>) {
    let server = accounting_server(handled, 0);
    let empty = chunked_answer(server.addr(), ACCOUNTED_ROUTE, &[]);
    assert_eq!(
        empty.status, 200,
        "a zero bound admits a body with no data frame"
    );
    assert_eq!(empty.body.as_ref(), b"");
    let admitted = server.controller().observed();
    assert_eq!(admitted.frames_polled, 0);
    assert_eq!(admitted.peak_retained_bytes, 0);

    let crossed = chunked_answer(server.addr(), ACCOUNTED_ROUTE, &[b"x"]);
    assert_eq!(
        crossed.status, 413,
        "a zero bound refuses the first data byte"
    );
    let refused = server.controller().observed();
    assert_eq!(refused.frames_polled, 1);
    assert_eq!(
        refused.peak_retained_bytes, 0,
        "a zero bound retains nothing at all"
    );
}

/// Assert a body that stops being readable is not reported as an oversized one.
fn assert_truncated_body_is_unreadable(handled: &Arc<AtomicUsize>) {
    let server = accounting_server(handled, DECLARED_CEILING);
    let before = handled.load(Ordering::SeqCst);
    let unreadable = unreadable_answer(server.addr(), ACCOUNTED_ROUTE);
    assert_eq!(
        unreadable.status, 400,
        "a body that stopped being readable is the transport's failure, not a size complaint"
    );
    assert_eq!(unreadable.text().as_ref(), "request body could not be read");
    assert_eq!(
        handled.load(Ordering::SeqCst),
        before,
        "an unreadable body reaches no handler"
    );
}

#[test]
fn buffered_checked_accounting_drops_crossing_frame_and_bounds_peak_retention() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            assert_retained_within_bound(&handled);
            assert_crossing_frame_is_dropped(&handled);
            assert_zero_bound_rows(&handled);
            assert_truncated_body_is_unreadable(&handled);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn declared_limit_refuses_before_buffered_collection() {
    common::test_runtime()
        .run(|| {
            let handled = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));

            let server = declaration_server(&handled, &calls, &drops);
            assert_declaration_refusals(&server, &calls, &handled);
            assert_zero_maximum_rows(&server, &calls, &handled);
            assert_admitted_declarations(&server, &calls, &handled);
            assert_hard_ceiling_refusal(&handled, &calls, &drops);

            assert_eq!(
                drops.load(Ordering::Acquire),
                1,
                "exactly one permit was handed over, and it was released once"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn buffered_permit_stays_owned_through_handler_and_drops_afterward() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let drops = Arc::new(AtomicUsize::new(0));
            let gate = Arc::new(tokio::sync::Notify::new());
            let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);

            let port = wire::reserve_request_body_owner();
            let mut router = Router::new();
            router.post("/held/:id", {
                let gate = Arc::clone(&gate);
                move |request: &Request| {
                    let gate = Arc::clone(&gate);
                    let entered_tx = entered_tx.clone();
                    let body: Box<str> = request.body().into();
                    Box::pin(async move {
                        entered_tx
                            .send(())
                            .expect("the case is still waiting for handler entry");
                        gate.notified().await;
                        Response::text(200, &body).expect("valid held status")
                    }) as Pin<Box<dyn Future<Output = Response> + Send>>
                }
            });
            router.post("/after/:id", |request: &Request| {
                let body: Box<str> = request.body().into();
                Box::pin(async move { Response::text(200, &body).expect("valid control status") })
                    as Pin<Box<dyn Future<Output = Response> + Send>>
            });
            let router = router.body_admission({
                let drops = Arc::clone(&drops);
                move |_context: &BodyAdmissionContext<'_>| {
                    Ok(BodyAdmission::with_permit(
                        4096,
                        DropProbe(Arc::clone(&drops)),
                    ))
                }
            });
            let server = port.serve(router);
            let addr = server.addr();

            let held =
                std::thread::spawn(move || wire::send(addr, "POST", "/held/1", &[], b"held-body"));
            entered
                .recv_timeout(HANDSHAKE_BOUND)
                .expect("the handler reported entry");
            assert_eq!(
                drops.load(Ordering::Acquire),
                0,
                "the permit stays owned while the handler holds its request"
            );
            assert_eq!(
                server.controller().observed().permit_owners_dropped,
                0,
                "no permit owner has been released yet"
            );

            gate.notify_one();
            let answered = held.join().expect("the held request completed");
            assert_eq!(answered.status, 200);
            assert_eq!(answered.body.as_ref(), b"held-body");
            assert_eq!(
                drops.load(Ordering::Acquire),
                1,
                "request and response completion releases the permit exactly once"
            );
            assert_eq!(server.controller().observed().permit_owners_dropped, 1);

            let control = wire::send(addr, "POST", "/after/2", &[], b"after-body");
            assert_eq!(control.status, 200);
            assert_eq!(control.body.as_ref(), b"after-body");
            assert_eq!(drops.load(Ordering::Acquire), 2);
            assert_eq!(server.controller().observed().permit_owners_dropped, 2);

            runtime::request_shutdown();
        })
        .unwrap();
}
