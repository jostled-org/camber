//! 1.T1: the public service-budget vocabulary validates and composes.

use camber::RuntimeError;
use camber::http::{
    ByteBoundary, DeadlineBoundary, ProxyPolicy, RequestBudget, ServerPolicy, TransferBudget,
};
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
