//! Route and host authority over streaming multipart, and the order of
//! everything that runs before the first payload frame.
//!
//! Every case here drives a real accepted HTTP request through the public
//! `Router` or `HostRouter`. The lifecycle observer supplies counters only: it
//! reads the production driver's own progress and the production permit owner's
//! own release, and chooses nothing.

use crate::http as wire;
use crate::rejection_support::{Journal, drain, journal, only, recording_mapper};
use crate::runtime_support as common;
use crate::streaming_multipart::{
    BOUNDARY, DECLARED, Field, content_type, multipart_body, reading_handler,
};

use camber::RuntimeError;
use camber::http::mock::LifecycleController;
use camber::http::{
    BodyAdmission, BodyAdmissionContext, HostRouter, Method, MultipartLimits, RejectionKind,
    Request, RequestBodyMode, Response, Router,
};
use camber::runtime;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// What one admission callback was given, and what had been read when it ran.
#[derive(Debug)]
struct Seen {
    route: Box<str>,
    raw_path: Box<str>,
    mode: RequestBodyMode,
    declared_length: Option<u64>,
    frames_at_entry: usize,
}

/// Every admission callback one fixture saw, in order.
type Callbacks = Arc<Mutex<Vec<Seen>>>;

fn callbacks() -> Callbacks {
    Arc::new(Mutex::new(Vec::new()))
}

/// Take everything the policy has recorded so far.
fn seen(journal: &Callbacks) -> Box<[Seen]> {
    std::mem::take(&mut *journal.lock().unwrap_or_else(|error| error.into_inner()))
        .into_boxed_slice()
}

/// The limits every row here registers unless it is about a bound of its own.
fn limits() -> MultipartLimits {
    MultipartLimits::builder()
        .max_fields(8)
        .max_field_bytes(64 * 1024)
        .max_chunk_bytes(1024)
        .max_parser_buffer_bytes(64 * 1024)
        .build()
        .expect("the fixture limits are a valid combination")
}

/// A policy that records what it was asked, and admits under `limit`.
///
/// The multipart frame count is read at callback entry, through the same
/// observer the case reads afterwards, so "before the first payload frame" is a
/// claim about the production driver rather than about the order two fixtures
/// happened to run in.
fn recording_policy(
    journal: &Callbacks,
    observer: &Arc<LifecycleController>,
    limit: usize,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let journal = Arc::clone(journal);
    let observer = Arc::clone(observer);
    move |context: &BodyAdmissionContext<'_>| {
        journal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Seen {
                route: context.route().into(),
                raw_path: context.raw_path().into(),
                mode: context.mode(),
                declared_length: context.declared_length(),
                frames_at_entry: observer.multipart_observed().body_frames_polled(),
            });
        Ok(BodyAdmission::new(limit))
    }
}

/// The body every routing row sends.
fn routed_body() -> Vec<u8> {
    multipart_body(
        BOUNDARY,
        &[
            Field::text("note", "hello"),
            Field::file("upload", "a.bin", "application/octet-stream", &[7u8; 40]),
        ],
    )
}

/// What a fully read fixture body describes itself as.
const ROUTED_FIELDS: &str = "note/-/-/5/1,upload/a.bin/application/octet-stream/40/1";

#[test]
fn multipart_host_routing_selects_one_child_plan_and_identity() {
    common::test_runtime()
        .run(|| {
            let port = wire::reserve_observed();
            let observer = port.controller();
            let alpha_seen = callbacks();
            let beta_seen = callbacks();
            let alpha_handled = Arc::new(AtomicUsize::new(0));
            let beta_handled = Arc::new(AtomicUsize::new(0));
            let mapped = journal();

            let mut alpha = Router::new();
            alpha.multipart(
                Method::Post,
                "/upload/:id",
                limits(),
                reading_handler("alpha", "id", &alpha_handled),
            );
            let alpha = alpha
                .body_admission(recording_policy(&alpha_seen, &observer, 8 * 1024))
                .rejection_mapper(recording_mapper(&mapped, "alpha"));

            let mut beta = Router::new();
            beta.multipart(
                Method::Put,
                "/files/:name",
                limits(),
                reading_handler("beta", "name", &beta_handled),
            );
            let beta = beta
                .body_admission(recording_policy(&beta_seen, &observer, 8 * 1024))
                .rejection_mapper(recording_mapper(&mapped, "beta"));

            let mut hosts = HostRouter::new();
            hosts.add("alpha.test", alpha);
            hosts.add("beta.test", beta);
            let server = port.serve_hosts(hosts);
            let addr = server.addr();

            assert_child_answer(addr, "POST", "/upload/a7", "alpha.test", "alpha|a7");
            // The frame counter belongs to the listener and both children share
            // it, so beta's "before its own first payload frame" claim is stated
            // against what alpha's answered request left behind rather than
            // against zero.
            let before_beta = observer.multipart_observed().body_frames_polled();
            assert_child_answer(addr, "PUT", "/files/report", "beta.test", "beta|report");
            assert_wrong_method_refusal(addr, &mapped);

            assert_selected_plan(&seen(&alpha_seen), &seen(&beta_seen), before_beta);
            assert_eq!(
                (
                    alpha_handled.load(Ordering::SeqCst),
                    beta_handled.load(Ordering::SeqCst)
                ),
                (1, 1),
                "each handler ran exactly once, and the refused method reached neither"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Assert one host child answered its own route with its own handler.
fn assert_child_answer(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    expected: &str,
) {
    let declared = content_type(BOUNDARY);
    let answered = wire::request_to_host_with_body(
        addr,
        method,
        path,
        host,
        &[("Content-Type", declared.as_ref())],
        &routed_body(),
    )
    .unwrap_or_else(|error| panic!("{method} {path} (Host: {host}) did not complete: {error}"));
    assert_eq!(answered.status, 200, "{host} answered its own route");
    assert_eq!(
        String::from_utf8_lossy(&answered.body),
        format!("{expected}|{ROUTED_FIELDS}"),
        "{host} supplied the handler, the parameter, and the limits once"
    );
}

/// Assert a method the multipart path does not serve keeps its existing
/// refusal: no multipart-specific matcher precedes the trie.
fn assert_wrong_method_refusal(addr: std::net::SocketAddr, mapped: &Journal) {
    let refused = wire::send_to_host(addr, "GET", "/upload/a7", "alpha.test");
    assert_eq!(refused.status, 405, "a wrong method keeps its refusal");
    let mismatch = only(mapped, "wrong method on a multipart path");
    assert_eq!(
        (mismatch.origin, mismatch.kind),
        ("alpha", RejectionKind::MethodSelection),
        "the selected child's own mapper answered: {mismatch:?}"
    );
    assert_eq!(
        mismatch.allow.as_deref(),
        Some("POST"),
        "the frozen allowed set names the registered method: {mismatch:?}"
    );
}

/// Assert exactly one child's plan answered for each request.
fn assert_selected_plan(alpha_calls: &[Seen], beta_calls: &[Seen], before_beta: usize) {
    assert_eq!(
        alpha_calls.len(),
        1,
        "one request invokes one policy once, and the refused method invokes none: {alpha_calls:?}"
    );
    assert_eq!(
        (
            alpha_calls[0].route.as_ref(),
            alpha_calls[0].raw_path.as_ref(),
            alpha_calls[0].mode,
            alpha_calls[0].frames_at_entry
        ),
        (
            "/upload/:id",
            "/upload/a7",
            RequestBodyMode::Streaming,
            0usize
        ),
        "the selected child's plan named its own route in streaming mode: {alpha_calls:?}"
    );
    assert!(
        alpha_calls[0].declared_length.is_some(),
        "the framed length the peer declared reaches the plan: {alpha_calls:?}"
    );
    assert_eq!(
        beta_calls.len(),
        1,
        "the other child's policy ran only for its own request: {beta_calls:?}"
    );
    assert!(
        before_beta > 0,
        "alpha's request left payload frames on this listener, so beta's entry \
         count is a claim rather than a trivial zero"
    );
    assert_eq!(
        (
            beta_calls[0].route.as_ref(),
            beta_calls[0].raw_path.as_ref(),
            beta_calls[0].mode,
            beta_calls[0].frames_at_entry
        ),
        (
            "/files/:name",
            "/files/report",
            RequestBodyMode::Streaming,
            before_beta
        ),
        "the other child's plan named its own route in streaming mode, before its \
         own first payload frame: {beta_calls:?}"
    );
}

/// What one pre-body row configures its admission policy to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Admission {
    /// Admit the work, supplying a permit whose release the row counts.
    Admit,
    /// Decline the work.
    Refuse,
    /// Fail while deciding.
    Panic,
}

/// What one pre-body row configures its middleware gate to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Gate {
    /// Pass the request through to the route.
    Pass,
    /// Answer instead of the route.
    Block,
}

/// One pre-body row: what the route is configured with, what the peer declared,
/// and what must have happened by the time the answer is on the wire.
struct PreBody {
    label: &'static str,
    admission: Admission,
    gate: Gate,
    /// Every `Content-Type` line the request carries, in the order it sends
    /// them. Empty is a request that declares no representation at all.
    declared: &'static [&'static str],
    /// The boundary this row's body is framed with.
    boundary: &'static str,
    /// The route's own boundary maximum.
    max_boundary: usize,
    status: u16,
    /// The category the selected mapper must classify this row under. `None` is
    /// a row no refusal was raised for.
    kind: Option<RejectionKind>,
    handled: usize,
}

/// The boundary that is one byte longer than the multipart protocol admits.
const OVERLONG_BOUNDARY: &str =
    "b0123456789012345678901234567890123456789012345678901234567890123456789";

/// A boundary the protocol allows and a route with a stricter maximum refuses.
const EXCESS_BOUNDARY: &str = "boundary-twenty-two-ch";

/// The status a blocking gate answers with, in its own response.
const GATE_STATUS: u16 = 418;

/// The complete pre-body table.
///
/// Every refusal row states the category its selected mapper must record, so a
/// row cannot pass by being refused for a reason other than its own.
const PRE_BODY: &[PreBody] = &[
    PreBody {
        label: "a passing gate over an admitted request reaches the handler",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &[DECLARED],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 200,
        kind: None,
        handled: 1,
    },
    PreBody {
        label: "the first declared representation is the one that is read",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &[DECLARED, "text/plain"],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 200,
        kind: None,
        handled: 1,
    },
    PreBody {
        label: "a declined admission answers before the gate or the handler",
        admission: Admission::Refuse,
        gate: Gate::Pass,
        declared: &[DECLARED],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 503,
        kind: Some(RejectionKind::BodyAdmission),
        handled: 0,
    },
    PreBody {
        label: "an admission that fails while deciding is Camber's own fault",
        admission: Admission::Panic,
        gate: Gate::Pass,
        declared: &[DECLARED],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 500,
        kind: Some(RejectionKind::InternalService),
        handled: 0,
    },
    PreBody {
        label: "a gate that answers instead of the route reaches no handler",
        admission: Admission::Admit,
        gate: Gate::Block,
        declared: &[DECLARED],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: GATE_STATUS,
        kind: None,
        handled: 0,
    },
    PreBody {
        label: "a request that declares no representation declares no boundary",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &[],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
    PreBody {
        label: "a multipart declaration with no boundary parameter",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &["multipart/form-data"],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
    PreBody {
        label: "a declaration that names the boundary twice",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &["multipart/form-data; boundary=CamberBnd7; boundary=CamberBnd7"],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
    PreBody {
        label: "a boundary whose quoting never closes",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &["multipart/form-data; boundary=\"CamberBnd7"],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
    PreBody {
        label: "a boundary longer than the protocol admits",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &["multipart/form-data; \
             boundary=b0123456789012345678901234567890123456789012345678901234567890123456789"],
        boundary: OVERLONG_BOUNDARY,
        max_boundary: 70,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
    PreBody {
        label: "a boundary longer than this route configured",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &["multipart/form-data; boundary=boundary-twenty-two-ch"],
        boundary: EXCESS_BOUNDARY,
        max_boundary: 8,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
    PreBody {
        label: "a representation that is not multipart at all",
        admission: Admission::Admit,
        gate: Gate::Pass,
        declared: &["application/json"],
        boundary: BOUNDARY,
        max_boundary: 70,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        handled: 0,
    },
];

/// The limits one pre-body row registers, under its own boundary maximum.
fn bounded_limits(max_boundary: usize) -> MultipartLimits {
    MultipartLimits::builder()
        .max_boundary_bytes(max_boundary)
        .build()
        .expect("the row's boundary maximum is a valid combination")
}

/// The policy one pre-body row registers.
fn row_policy(
    admission: Admission,
    released: &Arc<AtomicUsize>,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let released = Arc::clone(released);
    move |_context: &BodyAdmissionContext<'_>| match admission {
        Admission::Admit => Ok(BodyAdmission::with_permit(
            8 * 1024,
            wire::permit_probe(&released),
        )),
        Admission::Refuse => Err(RuntimeError::InvalidArgument("no capacity".into())),
        Admission::Panic => panic!("the policy failed while deciding"),
    }
}

/// What one pre-body row's request produced.
struct Answered {
    response: wire::HttpResponse,
    handled: usize,
    released: Arc<AtomicUsize>,
    mapped: Journal,
    observer: Arc<LifecycleController>,
}

/// Serve one pre-body row and answer its request.
fn drive_pre_body(row: &PreBody) -> Answered {
    let port = wire::reserve_observed();
    let observer = port.controller();
    let released = Arc::new(AtomicUsize::new(0));
    let handled = Arc::new(AtomicUsize::new(0));
    let mapped = journal();

    let mut router = Router::new();
    router.multipart(
        Method::Post,
        "/upload",
        bounded_limits(row.max_boundary),
        reading_handler("row", "absent", &handled),
    );
    match row.gate {
        Gate::Pass => {}
        Gate::Block => router.use_middleware(|_request: &Request, _next| async move {
            Response::text(GATE_STATUS, "the gate answered")
        }),
    }
    let router = router
        .body_admission(row_policy(row.admission, &released))
        .rejection_mapper(recording_mapper(&mapped, "row"));
    let server = port.serve(router);

    let declared: Box<[(&str, &str)]> = row
        .declared
        .iter()
        .map(|value| ("Content-Type", *value))
        .collect();
    let response = wire::request_to_host_with_body(
        server.addr(),
        "POST",
        "/upload",
        "localhost",
        &declared,
        &multipart_body(row.boundary, &[Field::text("note", "hello")]),
    )
    .unwrap_or_else(|error| panic!("{}: the row did not complete: {error}", row.label));
    Answered {
        response,
        handled: handled.load(Ordering::SeqCst),
        released,
        mapped,
        observer,
    }
}

#[test]
fn multipart_admission_gate_and_boundary_validation_precede_first_poll() {
    common::test_runtime()
        .run(|| {
            for row in PRE_BODY {
                let answered = drive_pre_body(row);
                assert_eq!(
                    answered.response.status, row.status,
                    "{}: answered with {}",
                    row.label, answered.response.status
                );
                assert_eq!(
                    answered.handled, row.handled,
                    "{}: handler entries",
                    row.label
                );
                assert_mapping(row, &answered.mapped);
                assert_pre_body_reads(row, &answered.observer);
                // Exactly one owner holds any admitted permit, and it releases
                // once: the count is the production owner's own drop.
                let owners = usize::from(row.admission == Admission::Admit);
                wire::assert_released(&answered.released, owners, row.label);
                wire::assert_owners_released(&answered.observer, owners, row.label);
            }

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Assert one row's refusal was classified once, by the route's own mapper.
fn assert_mapping(row: &PreBody, mapped: &Journal) {
    match row.kind {
        Some(kind) => {
            let observed = only(mapped, row.label);
            assert_eq!(
                (observed.origin, observed.kind),
                ("row", kind),
                "{}: the route's own mapper classified it once: {observed:?}",
                row.label
            );
            assert_eq!(
                observed.route.as_deref(),
                Some("/upload"),
                "{}: the refusal names the route classification established: {observed:?}",
                row.label
            );
        }
        None => {
            let observed = drain(mapped);
            assert!(
                observed.is_empty(),
                "{}: nothing was refused, so no mapper ran: {observed:?}",
                row.label
            );
        }
    }
}

/// Assert what this row's session read before its answer was selected.
fn assert_pre_body_reads(row: &PreBody, observer: &LifecycleController) {
    let observed = observer.multipart_observed();
    match row.handled {
        0 => assert_eq!(
            (observed.body_frames_polled(), observed.commands_accepted()),
            (0, 0),
            "{}: a request refused before its session exists polls nothing: {observed:?}",
            row.label
        ),
        _ => assert!(
            observed.body_frames_polled() > 0
                && observed.revocations() == 1
                && observed.drivers_terminated() == 1,
            "{}: an answered request read its payload and released one session: {observed:?}",
            row.label
        ),
    }
}
