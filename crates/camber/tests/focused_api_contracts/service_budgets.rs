//! 1.T1: the public service-budget vocabulary validates and composes.

#[cfg(feature = "profiling")]
use camber::__private::{DEFAULT_PROFILING_RESPONSE_LIMIT, frozen_profiling_response_limit};
use camber::__private::{
    DEFAULT_STATIC_FILE_LIMIT, frozen_buffered_response_limit, frozen_static_file_limit,
};
use camber::RuntimeError;
use camber::http::{
    ByteBoundary, DeadlineBoundary, HostRouter, ProxyPolicy, RequestBudget, Router, ServerPolicy,
    TransferBudget,
};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// The largest connection limit a listener's admission semaphore can hold.
const MAX_SERVABLE_CONNECTIONS: usize = tokio::sync::Semaphore::MAX_PERMITS;

/// The longest deadline a policy owner's clock can carry: thirty years, the
/// horizon Tokio's own timer stops at.
const MAX_SERVABLE_DEADLINE: Duration = Duration::from_secs(86_400 * 365 * 30);

const ZERO: Duration = Duration::ZERO;
const SHORT: Duration = Duration::from_secs(5);
const LONG: Duration = Duration::from_secs(20);

fn assert_policy_value<T: Copy + Clone + std::fmt::Debug + Eq + PartialEq + Send + Sync>() {}

fn expect_invalid(result: Result<impl std::fmt::Debug, RuntimeError>, expected_name: &str) {
    match result {
        Err(RuntimeError::InvalidArgument(message)) => assert!(
            message.contains(expected_name),
            "expected {expected_name} in refusal, got: {message}"
        ),
        other => panic!("expected InvalidArgument naming {expected_name}, got {other:?}"),
    }
}

/// Every closed deadline boundary, matched without a wildcard.
fn deadline_name(boundary: DeadlineBoundary) -> &'static str {
    match boundary {
        DeadlineBoundary::Header => "header",
        DeadlineBoundary::RequestBodyIdle => "request-body-idle",
        DeadlineBoundary::RequestTotal => "request-total",
        DeadlineBoundary::TransferIdle => "transfer-idle",
        DeadlineBoundary::TransferTotal => "transfer-total",
        DeadlineBoundary::ProxyConnect => "proxy-connect",
        DeadlineBoundary::ProxyRequest => "proxy-request",
        DeadlineBoundary::ProxyUpstreamIdle => "proxy-upstream-idle",
        DeadlineBoundary::ClientConnect => "client-connect",
        DeadlineBoundary::ClientRequest => "client-request",
        DeadlineBoundary::ClientResponseIdle => "client-response-idle",
        DeadlineBoundary::ResourceStartupHealth => "resource-startup-health",
        DeadlineBoundary::ResourcePeriodicHealth => "resource-periodic-health",
        DeadlineBoundary::ResourceShutdown => "resource-shutdown",
        DeadlineBoundary::AggregateShutdown => "aggregate-shutdown",
    }
}

/// Every closed byte boundary, matched without a wildcard.
fn byte_name(boundary: ByteBoundary) -> &'static str {
    match boundary {
        ByteBoundary::RequestBody => "request-body",
        ByteBoundary::TransferUpload => "transfer-upload",
        ByteBoundary::TransferDownload => "transfer-download",
        ByteBoundary::ClientResponse => "client-response",
        ByteBoundary::ProxyBufferedResponse => "proxy-buffered-response",
        ByteBoundary::StaticFile => "static-file",
        ByteBoundary::ProfilingResponse => "profiling-response",
    }
}

fn assert_request_budget_contract() {
    let bounded = RequestBudget::bounded(SHORT, LONG).expect("finite request budget");
    assert_eq!(bounded.body_idle(), Some(SHORT));
    assert_eq!(bounded.total(), Some(LONG));

    // The two dimensions are independent: removing one leaves the other whole.
    assert_eq!(bounded.without_body_idle().body_idle(), None);
    assert_eq!(bounded.without_body_idle().total(), Some(LONG));
    assert_eq!(bounded.without_total().body_idle(), Some(SHORT));
    assert_eq!(bounded.without_total().total(), None);

    let unbounded = RequestBudget::unbounded();
    assert_eq!(unbounded.body_idle(), None);
    assert_eq!(unbounded.total(), None);
    assert_eq!(
        unbounded
            .with_body_idle(SHORT)
            .and_then(|budget| budget.with_total(LONG))
            .expect("named setters build the finite budget"),
        bounded,
    );

    // Zero is never a spelling of unbounded.
    expect_invalid(RequestBudget::bounded(ZERO, LONG), "body_idle");
    expect_invalid(RequestBudget::bounded(SHORT, ZERO), "total");
    expect_invalid(bounded.with_body_idle(ZERO), "body_idle");
    expect_invalid(bounded.with_total(ZERO), "total");
}

fn assert_transfer_budget_contract() {
    let bounded = TransferBudget::bounded(1024, SHORT, LONG).expect("finite transfer budget");
    assert_eq!(bounded.max_bytes(), Some(1024));
    assert_eq!(bounded.idle(), Some(SHORT));
    assert_eq!(bounded.total(), Some(LONG));

    assert_eq!(bounded.without_max_bytes().max_bytes(), None);
    assert_eq!(bounded.without_max_bytes().idle(), Some(SHORT));
    assert_eq!(bounded.without_idle().idle(), None);
    assert_eq!(bounded.without_idle().max_bytes(), Some(1024));
    assert_eq!(bounded.without_total().total(), None);
    assert_eq!(bounded.without_total().idle(), Some(SHORT));

    let unbounded = TransferBudget::unbounded();
    assert_eq!(unbounded.max_bytes(), None);
    assert_eq!(unbounded.idle(), None);
    assert_eq!(unbounded.total(), None);
    assert_eq!(
        unbounded
            .with_max_bytes(1024)
            .and_then(|budget| budget.with_idle(SHORT))
            .and_then(|budget| budget.with_total(LONG))
            .expect("named setters build the finite budget"),
        bounded,
    );

    expect_invalid(TransferBudget::bounded(0, SHORT, LONG), "max_bytes");
    expect_invalid(TransferBudget::bounded(1024, ZERO, LONG), "idle");
    expect_invalid(TransferBudget::bounded(1024, SHORT, ZERO), "total");
    expect_invalid(bounded.with_max_bytes(0), "max_bytes");
    expect_invalid(bounded.with_idle(ZERO), "idle");
    expect_invalid(bounded.with_total(ZERO), "total");
}

/// Every finite connection limit a listener can serve, and every one it cannot.
///
/// An omitted limit is unbounded, and unbounded is not zero. No finite limit
/// spells it, not the smallest and not the largest servable one. A limit above
/// what the admission semaphore holds is refused by the policy setter, not by
/// the listener that would panic on it.
fn assert_connection_limit_contract(default: ServerPolicy) {
    assert_ne!(
        default,
        default.connection_limit(1).expect("positive limit")
    );
    assert_ne!(
        default,
        default
            .connection_limit(MAX_SERVABLE_CONNECTIONS)
            .expect("largest servable limit"),
    );
    assert_ne!(
        default.connection_limit(1).expect("positive limit"),
        default
            .connection_limit(MAX_SERVABLE_CONNECTIONS)
            .expect("largest servable limit"),
    );
    expect_invalid(default.connection_limit(0), "connection_limit");
    expect_invalid(
        default.connection_limit(MAX_SERVABLE_CONNECTIONS + 1),
        "connection_limit",
    );
    expect_invalid(default.connection_limit(usize::MAX), "connection_limit");

    // The accepted ceiling is the admission owner's own: the semaphore every
    // listener builds from a finite limit holds exactly this many permits and
    // panics on more.
    assert_eq!(
        tokio::sync::Semaphore::new(MAX_SERVABLE_CONNECTIONS).available_permits(),
        MAX_SERVABLE_CONNECTIONS,
        "the largest accepted limit must be one the admission semaphore holds",
    );
}

/// Every deadline a policy owner's clock can carry, and every one it cannot.
///
/// A finite deadline becomes `Instant::now() + value` at the owner enforcing
/// it, and that addition panics rather than saturating on a sum the platform
/// cannot represent. Every dimension shares one ceiling because every dimension
/// ends at that same addition, and the setter refuses what the clock cannot
/// reach — not the supervisor that would panic on it.
fn assert_deadline_ceiling_contract() {
    let server = ServerPolicy::default();
    let proxy = ProxyPolicy::default();
    let over = MAX_SERVABLE_DEADLINE + Duration::from_secs(1);

    assert_ne!(
        server,
        server
            .header_timeout(MAX_SERVABLE_DEADLINE)
            .expect("the longest carried deadline"),
    );
    assert_ne!(
        server,
        server
            .shutdown_timeout(MAX_SERVABLE_DEADLINE)
            .expect("the longest carried deadline"),
    );

    expect_invalid(server.header_timeout(over), "header_timeout");
    expect_invalid(server.shutdown_timeout(over), "shutdown_timeout");
    expect_invalid(server.shutdown_timeout(Duration::MAX), "shutdown_timeout");
    expect_invalid(RequestBudget::bounded(over, LONG), "body_idle");
    expect_invalid(RequestBudget::bounded(SHORT, Duration::MAX), "total");
    expect_invalid(TransferBudget::bounded(1024, over, LONG), "idle");
    expect_invalid(TransferBudget::bounded(1024, SHORT, Duration::MAX), "total");
    expect_invalid(proxy.connect_timeout(over), "connect_timeout");
    expect_invalid(proxy.request_timeout(Duration::MAX), "request_timeout");
    expect_invalid(proxy.upstream_idle_timeout(over), "upstream_idle_timeout");

    // The accepted ceiling is the enforcing clock's own. `tokio::time::Instant`
    // adds through the `std` instant below, so this is the addition every
    // deadline owner performs.
    assert!(
        Instant::now().checked_add(MAX_SERVABLE_DEADLINE).is_some(),
        "the longest accepted deadline must be one the clock can reach",
    );
    assert!(
        Instant::now().checked_add(Duration::MAX).is_none(),
        "a refused deadline must be one the clock cannot reach",
    );
}

fn assert_server_policy_contract() {
    let default = ServerPolicy::default();

    // Documented defaults, read back through the setters that write them.
    assert_eq!(
        default,
        default
            .header_timeout(Duration::from_secs(60))
            .expect("default header timeout")
            .shutdown_timeout(Duration::from_secs(30))
            .expect("default shutdown timeout")
            .request_budget(
                RequestBudget::bounded(Duration::from_secs(30), Duration::from_secs(30))
                    .expect("default request budget"),
            )
            .upload_budget(TransferBudget::unbounded())
            .download_budget(TransferBudget::unbounded()),
    );

    assert_connection_limit_contract(default);
    expect_invalid(default.header_timeout(ZERO), "header_timeout");
    expect_invalid(default.shutdown_timeout(ZERO), "shutdown_timeout");

    // Each dimension is written independently: setting one leaves the rest.
    let narrowed = default
        .header_timeout(SHORT)
        .expect("finite header timeout")
        .connection_limit(8)
        .expect("positive limit");
    assert_eq!(
        narrowed,
        default
            .connection_limit(8)
            .expect("positive limit")
            .header_timeout(SHORT)
            .expect("finite header timeout"),
    );

    #[cfg(feature = "profiling")]
    {
        // The published eight-MiB ceiling, read back through the setter that
        // writes it.
        assert_eq!(
            default,
            default
                .profiling_response_limit(8 * 1024 * 1024)
                .expect("default profiling response limit"),
        );
        expect_invalid(
            default.profiling_response_limit(0),
            "profiling_response_limit",
        );
        assert_ne!(
            default,
            default
                .profiling_response_limit(1024)
                .expect("positive profiling limit"),
        );
        // The rendered profile has an explicit opt-out of its own, and it is a
        // different value from every finite ceiling.
        assert_ne!(default, default.unbounded_profiling_response());
    }
}

fn assert_proxy_policy_contract() {
    let default = ProxyPolicy::default();
    assert_eq!(
        default,
        default
            .connect_timeout(Duration::from_secs(30))
            .expect("default connect timeout")
            .request_timeout(Duration::from_secs(30))
            .expect("default request timeout")
            .upstream_idle_timeout(Duration::from_secs(30))
            .expect("default upstream idle timeout")
            .buffered_response_limit(8 * 1024 * 1024)
            .expect("default buffered maximum"),
    );

    // The three phases are independent, and the buffered ceiling is opt-out.
    assert_ne!(
        default.connect_timeout(SHORT).expect("finite connect"),
        default.request_timeout(SHORT).expect("finite request"),
    );
    assert_ne!(default, default.unbounded_buffered_response());
    assert_eq!(
        default
            .unbounded_buffered_response()
            .buffered_response_limit(8 * 1024 * 1024)
            .expect("restored buffered maximum"),
        default,
    );

    expect_invalid(default.connect_timeout(ZERO), "connect_timeout");
    expect_invalid(default.request_timeout(ZERO), "request_timeout");
    expect_invalid(default.upstream_idle_timeout(ZERO), "upstream_idle_timeout");
    expect_invalid(
        default.buffered_response_limit(0),
        "buffered_response_limit",
    );

    // The two streaming budgets are dimensions of their own. Both default to
    // unbounded, each writes only its own direction, and neither is the
    // buffered ceiling — a proxy that streams is bounded by these, not by it.
    let finite = TransferBudget::unbounded()
        .with_max_bytes(4096)
        .expect("a finite transfer maximum");
    let uploading = default.upload_budget(finite);
    let downloading = default.download_budget(finite);
    assert_ne!(default, uploading);
    assert_ne!(default, downloading);
    assert_ne!(uploading, downloading);
    assert_eq!(
        uploading.download_budget(finite),
        downloading.upload_budget(finite),
    );
    assert_eq!(
        default.upload_budget(TransferBudget::unbounded()),
        default,
        "an explicitly unbounded upload budget is the documented default",
    );
    assert_eq!(
        default.download_budget(TransferBudget::unbounded()),
        default,
        "an explicitly unbounded download budget is the documented default",
    );
    assert_eq!(
        uploading
            .buffered_response_limit(1024)
            .expect("a finite buffered ceiling"),
        default
            .buffered_response_limit(1024)
            .expect("a finite buffered ceiling")
            .upload_budget(finite),
    );
}

/// The buffered response maximum every client starts with.
const DEFAULT_CLIENT_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// The deadlines every client starts with, on both response dimensions.
const DEFAULT_CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The smallest deadline a client builder's infallible setters will hold.
const MIN_CLIENT_DEADLINE: Duration = Duration::from_millis(1);

/// 7.T2
#[test]
fn buffered_client_requires_a_limit_or_explicit_unbounded_name() {
    let default = camber::http::client().response_policy();

    // The documented default, read back through the setter that writes it.
    assert_eq!(default.max_bytes(), Some(DEFAULT_CLIENT_RESPONSE_LIMIT));
    assert_eq!(default.idle(), Some(DEFAULT_CLIENT_TIMEOUT));
    assert_eq!(default.total(), Some(DEFAULT_CLIENT_TIMEOUT));
    assert_eq!(
        camber::http::client()
            .response_budget(
                TransferBudget::bounded(
                    DEFAULT_CLIENT_RESPONSE_LIMIT,
                    DEFAULT_CLIENT_TIMEOUT,
                    DEFAULT_CLIENT_TIMEOUT,
                )
                .expect("the default client response policy"),
            )
            .response_policy(),
        default,
    );

    // A finite ceiling is written whole, and it is the only dimension the
    // caller stated.
    let finite = TransferBudget::bounded(1024, SHORT, LONG).expect("a finite response policy");
    assert_eq!(
        camber::http::client()
            .response_budget(finite)
            .response_policy(),
        finite,
    );

    // Zero is refused where the value is built, so no client is ever
    // constructed holding it — and zero is never a spelling of unbounded.
    expect_invalid(finite.with_max_bytes(0), "max_bytes");
    expect_invalid(TransferBudget::bounded(0, SHORT, LONG), "max_bytes");
    assert_eq!(
        camber::http::client()
            .request_timeout(ZERO)
            .response_policy()
            .total(),
        Some(MIN_CLIENT_DEADLINE),
    );
    assert_eq!(
        camber::http::client()
            .response_idle_timeout(ZERO)
            .response_policy()
            .idle(),
        Some(MIN_CLIENT_DEADLINE),
    );

    // Only the explicitly named opt-out removes the ceiling, and it removes
    // nothing else.
    let unbounded = camber::http::client()
        .unbounded_response()
        .response_policy();
    assert_eq!(unbounded.max_bytes(), None);
    assert_eq!(unbounded.idle(), default.idle());
    assert_eq!(unbounded.total(), default.total());
    assert_eq!(
        camber::http::client()
            .request_timeout(SHORT)
            .response_idle_timeout(SHORT)
            .response_policy()
            .max_bytes(),
        Some(DEFAULT_CLIENT_RESPONSE_LIMIT),
        "no deadline setter may remove the response ceiling",
    );
}

/// The buffered upstream maximum every proxy route starts with.
const DEFAULT_PROXY_BUFFERED_LIMIT: usize = 8 * 1024 * 1024;

/// The finite ceiling one registered route names instead of the default.
const NAMED_PROXY_BUFFERED_LIMIT: usize = 1024;

/// A backend no row connects to: every registration here is frozen and dropped.
const UNREACHED_BACKEND: &str = "http://127.0.0.1:1";

/// Every policy dimension that is not the buffered ceiling.
///
/// A row reads the frozen ceiling before and after each one, so a setter that
/// reached the buffered dimension is caught by the dimension it did not name
/// rather than by a whole-value comparison that cannot say which field moved.
fn policies_written_beside_the_ceiling(base: ProxyPolicy) -> Box<[ProxyPolicy]> {
    let finite_transfer = TransferBudget::unbounded()
        .with_max_bytes(4096)
        .expect("a finite transfer maximum");
    Box::new([
        base.connect_timeout(SHORT).expect("a finite connect"),
        base.request_timeout(SHORT).expect("a finite request"),
        base.upstream_idle_timeout(SHORT)
            .expect("a finite upstream idle"),
        base.upload_budget(finite_transfer),
        base.download_budget(finite_transfer),
    ])
}

/// 8.T1
#[test]
fn buffered_proxy_requires_a_limit_or_explicit_unbounded_name() {
    // Zero is refused where the value is built, so no route can freeze it and
    // no registration ever sees it. Validation precedes the freeze because the
    // freeze takes a value only validation produces.
    expect_invalid(
        ProxyPolicy::default().buffered_response_limit(0),
        "buffered_response_limit",
    );

    // The documented default, read through the same accessor a route freezes.
    let default = ProxyPolicy::default();
    assert_eq!(
        frozen_buffered_response_limit(&default),
        Some(DEFAULT_PROXY_BUFFERED_LIMIT),
    );

    let finite = default
        .buffered_response_limit(NAMED_PROXY_BUFFERED_LIMIT)
        .expect("a finite buffered ceiling");
    assert_eq!(
        frozen_buffered_response_limit(&finite),
        Some(NAMED_PROXY_BUFFERED_LIMIT),
    );

    // Only the explicitly named opt-out removes the ceiling.
    let opted_out = default.unbounded_buffered_response();
    assert_eq!(frozen_buffered_response_limit(&opted_out), None);

    for beside in policies_written_beside_the_ceiling(default) {
        assert_eq!(
            frozen_buffered_response_limit(&beside),
            Some(DEFAULT_PROXY_BUFFERED_LIMIT),
            "no dimension outside the ceiling may remove it",
        );
    }
    for beside in policies_written_beside_the_ceiling(opted_out) {
        assert_eq!(
            frozen_buffered_response_limit(&beside),
            None,
            "no dimension outside the ceiling may restore it",
        );
    }

    // Every exported buffered-proxy registration takes one of those values, and
    // the concise spellings freeze the documented default.
    let healthy = Arc::new(AtomicBool::new(true));
    let mut routed = Router::new();
    routed.proxy("/default", UNREACHED_BACKEND);
    routed.proxy_checked("/default-checked", UNREACHED_BACKEND, Arc::clone(&healthy));
    routed.proxy_with_policy("/finite", UNREACHED_BACKEND, finite);
    routed.proxy_with_policy("/unbounded", UNREACHED_BACKEND, opted_out);
    routed.proxy_checked_with_policy("/checked", UNREACHED_BACKEND, Arc::clone(&healthy), finite);

    // The same registrations reach a host router through the router it selects.
    let mut hosts = HostRouter::new();
    hosts.add("bounded.example", routed);
}

/// 1.T1
#[test]
fn budget_constructors_validate_every_finite_value_and_explicit_unbounded_choice() {
    assert_request_budget_contract();
    assert_transfer_budget_contract();
    assert_server_policy_contract();
    assert_proxy_policy_contract();
    assert_deadline_ceiling_contract();

    // Small immutable policy values are copied, not shared.
    assert_policy_value::<RequestBudget>();
    assert_policy_value::<TransferBudget>();
    assert_policy_value::<ServerPolicy>();
    assert_policy_value::<ProxyPolicy>();
    assert_policy_value::<DeadlineBoundary>();
    assert_policy_value::<ByteBoundary>();

    // Both boundary vocabularies are closed, so a caller can match every value.
    assert_eq!(deadline_name(DeadlineBoundary::Header), "header");
    assert_eq!(
        deadline_name(DeadlineBoundary::AggregateShutdown),
        "aggregate-shutdown"
    );
    assert_eq!(
        byte_name(ByteBoundary::ProfilingResponse),
        "profiling-response"
    );
    assert_eq!(byte_name(ByteBoundary::RequestBody), "request-body");
}

/// A root no row reads from.
///
/// Every direct call here is refused before it owns a path, so the directory
/// this names never has to exist: a row that reached the filesystem would find
/// it missing and report a 404 instead of the refusal it asserts.
const UNREAD_STATIC_ROOT: &str = "/camber-static-file-contract";

/// The file every static-file row asks for, and none of them opens.
const UNREAD_STATIC_FILE: &str = "asset.txt";

/// One static-file future's answer, taken with no executor to hand it to.
///
/// The claim each row makes is that its refusal is decided before any
/// filesystem work, so the future is given exactly one turn on the caller's own
/// stack. A future that needed a second turn — or a runtime — reports `Pending`
/// here rather than the refusal the row names.
fn first_turn<T>(future: impl Future<Output = T>) -> Poll<T> {
    let mut future = std::pin::pin!(future);
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
}

/// Assert one static-file entry point refuses for the absent runtime alone.
///
/// The rows that pass a valid maximum all stop at the same place, and naming
/// that place once is what says the maximum they carried was accepted: a
/// ceiling refused earlier would never reach it.
fn expect_no_runtime(answer: Poll<Result<camber::http::Response, RuntimeError>>, named: &str) {
    match answer {
        Poll::Ready(Err(RuntimeError::NoRuntime)) => {}
        other => panic!("expected {named} to refuse an absent runtime, got {other:?}"),
    }
}

/// 9.T1
#[test]
fn static_file_requires_a_limit_or_explicit_unbounded_name() {
    let root = Path::new(UNREAD_STATIC_ROOT);

    // Zero is refused where the maximum is frozen, which is before a path is
    // owned, a worker starts, or a runtime is asked for.
    expect_invalid(frozen_static_file_limit(0), "max_bytes");
    assert_eq!(
        frozen_static_file_limit(1024).expect("a finite ceiling"),
        Some(1024),
    );
    assert_eq!(DEFAULT_STATIC_FILE_LIMIT, 8 * 1024 * 1024);

    // The direct entry that names a maximum refuses zero on its first turn,
    // ahead of the runtime check every other row stops at.
    match first_turn(camber::http::serve_file_with_limit(
        root,
        UNREAD_STATIC_FILE,
        0,
    )) {
        Poll::Ready(Err(RuntimeError::InvalidArgument(message))) => assert!(
            message.contains("max_bytes"),
            "expected max_bytes in refusal, got: {message}"
        ),
        other => panic!("expected a zero static-file maximum to be refused, got {other:?}"),
    }

    expect_no_runtime(
        first_turn(camber::http::serve_file(root, UNREAD_STATIC_FILE)),
        "serve_file",
    );
    expect_no_runtime(
        first_turn(camber::http::serve_file_with_limit(
            root,
            UNREAD_STATIC_FILE,
            1024,
        )),
        "serve_file_with_limit",
    );
    expect_no_runtime(
        first_turn(camber::http::serve_file_unbounded(root, UNREAD_STATIC_FILE)),
        "serve_file_unbounded",
    );

    // The routed spellings freeze the same three choices, and the zero one is
    // refused at registration rather than at the first request.
    let mut routed = Router::new();
    routed.static_files("/default", UNREAD_STATIC_ROOT);
    routed
        .static_files_with_limit("/finite", UNREAD_STATIC_ROOT, 1024)
        .expect("a finite routed ceiling");
    routed.static_files_unbounded("/unbounded", UNREAD_STATIC_ROOT);
    expect_invalid(
        routed.static_files_with_limit("/zero", UNREAD_STATIC_ROOT, 0),
        "max_bytes",
    );

    // The same registrations reach a host router through the router it selects.
    let mut hosts = HostRouter::new();
    hosts.add("static.example", routed);
}

/// Every `ServerPolicy` dimension that is not the rendered-profile ceiling.
///
/// A row reads the frozen ceiling after each one, so a setter that reached the
/// profiling dimension is caught by the dimension it did not name rather than by
/// a whole-value comparison that cannot say which field moved.
#[cfg(feature = "profiling")]
fn policies_written_beside_the_profiling_ceiling(base: ServerPolicy) -> Box<[ServerPolicy]> {
    Box::new([
        base.header_timeout(SHORT)
            .expect("a finite header boundary"),
        base.request_budget(RequestBudget::bounded(SHORT, LONG).expect("a finite request budget")),
        base.upload_budget(TransferBudget::unbounded()),
        base.download_budget(TransferBudget::unbounded()),
        base.shutdown_timeout(SHORT)
            .expect("a finite shutdown deadline"),
        base.connection_limit(8).expect("a finite connection limit"),
    ])
}

/// 10.T1
#[cfg(feature = "profiling")]
#[test]
fn profiling_output_requires_a_limit_or_explicit_unbounded_name() {
    let default = ServerPolicy::default();

    // Zero is refused where the maximum is written, so no policy can carry it to
    // a server and no profiler entry is ever reached under it.
    expect_invalid(
        default.profiling_response_limit(0),
        "profiling_response_limit",
    );

    // The documented default, read through the accessor a served profiling
    // route freezes its ceiling through.
    assert_eq!(DEFAULT_PROFILING_RESPONSE_LIMIT, 8 * 1024 * 1024);
    assert_eq!(
        frozen_profiling_response_limit(&default),
        Some(DEFAULT_PROFILING_RESPONSE_LIMIT),
    );

    let finite = default
        .profiling_response_limit(1024)
        .expect("a finite profiling ceiling");
    assert_eq!(frozen_profiling_response_limit(&finite), Some(1024));

    // Only the explicitly named opt-out removes the ceiling, and naming a
    // maximum again restores it.
    let opted_out = default.unbounded_profiling_response();
    assert_eq!(frozen_profiling_response_limit(&opted_out), None);
    assert_eq!(
        opted_out
            .profiling_response_limit(DEFAULT_PROFILING_RESPONSE_LIMIT)
            .expect("the restored documented maximum"),
        default,
    );

    for beside in policies_written_beside_the_profiling_ceiling(default) {
        assert_eq!(
            frozen_profiling_response_limit(&beside),
            Some(DEFAULT_PROFILING_RESPONSE_LIMIT),
            "no dimension outside the ceiling may remove it",
        );
    }
    for beside in policies_written_beside_the_profiling_ceiling(opted_out) {
        assert_eq!(
            frozen_profiling_response_limit(&beside),
            None,
            "no dimension outside the ceiling may restore it",
        );
    }
}
