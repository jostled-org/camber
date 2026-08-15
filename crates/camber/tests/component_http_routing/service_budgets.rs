//! 1.T2: runtime, server, host, and child policies narrow, and never widen.

use crate::http as http_support;

use camber::http::mock::{LifecycleCheckpoint, LifecycleController};
use camber::http::{
    HostRouter, Request, RequestBudget, Response, Router, ServerPolicy, TransferBudget,
};
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The runtime `#[camber::test]` establishes bounds every server below.
const RUNTIME_HEADER_TIMEOUT: Duration = Duration::from_millis(100);
/// The runtime's default request budget, which no test row configures away.
const RUNTIME_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One row of the precedence table: what was configured, and what must reach
/// the production owners that resolve it.
struct PolicyRow {
    name: &'static str,
    policy: ServerPolicy,
    expected_header: Duration,
    expected_request: RequestBudget,
    expected_upload: TransferBudget,
    expected_download: TransferBudget,
}

fn budget_route() -> Router {
    let mut router = Router::new();
    router.get("/budgets", |_req: &Request| async {
        Response::text(200, "budgets")
    });
    router
}

async fn wait_paused(controller: &LifecycleController, checkpoint: LifecycleCheckpoint, row: &str) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap_or_else(|_| panic!("{row}: {checkpoint:?} was never reached"))
        .unwrap_or_else(|error| panic!("{row}: waiting for {checkpoint:?} failed: {error}"));
}

/// Serve one row and prove the production owners resolved exactly its values.
///
/// Both checkpoints are armed before anything connects, and each is armed with
/// the exact value expected: a server that resolved a different header timeout
/// or a different budget never pauses, and the bounded wait reports that as the
/// failure it is. The values are read from the connection owner that configures
/// Hyper and from the routing owner that resolves the budgets — no helper here
/// recomputes either.
async fn assert_row(row: PolicyRow, hosts: Option<HostRouter>) {
    let port = http_support::reserve_observed();
    let (listener, addr, controller) = port.into_owned_parts();

    controller
        .pause_once(LifecycleCheckpoint::HeaderTimeoutConfigured(
            row.expected_header,
        ))
        .expect("arm the header-timeout observation");
    controller
        .pause_once(LifecycleCheckpoint::RouteBudgetsResolved {
            request: row.expected_request,
            upload: row.expected_upload,
            download: row.expected_download,
        })
        .expect("arm the resolved-budget observation");

    let handle = match hosts {
        Some(hosts) => camber::http::server_hosts(hosts).policy(row.policy),
        None => camber::http::server(budget_route()).policy(row.policy),
    }
    .serve_background(listener)
    .expect("owned server requires a Tokio runtime");
    let server = http_support::ReadyServer::adopt(addr, handle);

    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("http://{addr}/budgets"))
            .send()
            .await
    });

    wait_paused(
        &controller,
        LifecycleCheckpoint::HeaderTimeoutConfigured(row.expected_header),
        row.name,
    )
    .await;
    controller
        .release(LifecycleCheckpoint::HeaderTimeoutConfigured(
            row.expected_header,
        ))
        .expect("release the header-timeout observation");

    let budgets = LifecycleCheckpoint::RouteBudgetsResolved {
        request: row.expected_request,
        upload: row.expected_upload,
        download: row.expected_download,
    };
    wait_paused(&controller, budgets, row.name).await;
    controller
        .release(budgets)
        .expect("release the resolved-budget observation");

    let response = tokio::time::timeout(EVENT_TIMEOUT, request)
        .await
        .unwrap_or_else(|_| panic!("{}: the probe never completed", row.name))
        .unwrap_or_else(|error| panic!("{}: the probe task failed: {error}", row.name))
        .unwrap_or_else(|error| panic!("{}: the probe request failed: {error}", row.name));
    assert_eq!(response.status().as_u16(), 200, "{}", row.name);

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{}: teardown failed: {error}", row.name));
}

/// A server that configures nothing inherits the runtime's bounds, and its own
/// longer defaults cannot widen them.
async fn assert_server_inherits_the_runtime() {
    assert_row(
        PolicyRow {
            name: "inherit the runtime",
            policy: ServerPolicy::default(),
            expected_header: RUNTIME_HEADER_TIMEOUT,
            expected_request: RequestBudget::bounded(
                RUNTIME_REQUEST_TIMEOUT,
                RUNTIME_REQUEST_TIMEOUT,
            )
            .expect("the runtime's default request budget"),
            expected_upload: TransferBudget::unbounded(),
            expected_download: TransferBudget::unbounded(),
        },
        None,
    )
    .await;
}

/// A server narrows every dimension it names, and a longer one it names is
/// still capped by the runtime's.
async fn assert_server_narrows_the_runtime() {
    assert_row(
        PolicyRow {
            name: "server narrows",
            policy: ServerPolicy::default()
                .header_timeout(Duration::from_secs(30))
                .expect("a header timeout wider than the runtime's")
                .request_budget(
                    RequestBudget::bounded(Duration::from_secs(10), Duration::from_secs(20))
                        .expect("a narrower request budget"),
                )
                .upload_budget(
                    TransferBudget::unbounded()
                        .with_max_bytes(4096)
                        .expect("a finite upload maximum"),
                ),
            expected_header: RUNTIME_HEADER_TIMEOUT,
            expected_request: RequestBudget::bounded(
                Duration::from_secs(10),
                Duration::from_secs(20),
            )
            .expect("the server's request budget"),
            expected_upload: TransferBudget::unbounded()
                .with_max_bytes(4096)
                .expect("the server's upload maximum"),
            expected_download: TransferBudget::unbounded(),
        },
        None,
    )
    .await;
}

/// Host and child layers narrow further, per dimension, and an explicitly
/// unbounded inner value inherits rather than erasing what contains it.
async fn assert_host_and_child_narrow_the_server() {
    let child = budget_route()
        .request_budget(RequestBudget::unbounded())
        .upload_budget(
            TransferBudget::unbounded()
                .with_max_bytes(1024)
                .expect("the child's upload maximum"),
        )
        .download_budget(
            TransferBudget::unbounded()
                .with_idle(Duration::from_secs(2))
                .expect("the child's download idle bound"),
        );
    let mut hosts = HostRouter::new();
    hosts.set_default(child);
    let hosts = hosts
        .request_budget(
            RequestBudget::unbounded()
                .with_total(Duration::from_secs(5))
                .expect("the host's request total"),
        )
        .upload_budget(
            TransferBudget::unbounded()
                .with_max_bytes(2048)
                .expect("the host's upload maximum"),
        );

    assert_row(
        PolicyRow {
            name: "host and child narrow",
            policy: ServerPolicy::default()
                .request_budget(
                    RequestBudget::bounded(Duration::from_secs(10), Duration::from_secs(20))
                        .expect("the server's request budget"),
                )
                .upload_budget(
                    TransferBudget::unbounded()
                        .with_max_bytes(4096)
                        .expect("the server's upload maximum"),
                ),
            expected_header: RUNTIME_HEADER_TIMEOUT,
            // body_idle: only the server named one. total: the host's five
            // seconds beat the server's twenty, and the child's unbounded
            // request budget erased neither.
            expected_request: RequestBudget::bounded(
                Duration::from_secs(10),
                Duration::from_secs(5),
            )
            .expect("the narrowed request budget"),
            // The smallest finite maximum in the chain wins.
            expected_upload: TransferBudget::unbounded()
                .with_max_bytes(1024)
                .expect("the child's upload maximum"),
            // Nothing above the child bounded the download, so its own idle
            // bound stands alone.
            expected_download: TransferBudget::unbounded()
                .with_idle(Duration::from_secs(2))
                .expect("the child's download idle bound"),
        },
        Some(hosts),
    )
    .await;
}

/// 1.T2
#[camber::test]
async fn nested_policies_only_narrow_outer_finite_limits() {
    assert_server_inherits_the_runtime().await;
    assert_server_narrows_the_runtime().await;
    assert_host_and_child_narrow_the_server().await;
}
