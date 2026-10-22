//! Which server exposes Camber's `/health`, at which path, and what it refuses.
//!
//! The runtime owns the resource registry and every health check. Each server
//! decides whether it exposes the route, so a service can keep its own health
//! contract and still register resources for their lifecycle.

use crate::common;

use camber::http::{self, HealthRoute, HostRouter, Request, Response, Router, ServerPolicy};
use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::time::Duration;

const SERVICE_ANSWER: &str = "service-owned";

/// A runtime under `policy` with the bounds every resource test serves under.
///
/// The policy is written first because it replaces the whole outer policy, and
/// the two test bounds after it are single-field writes onto that value.
fn runtime_with(policy: ServerPolicy) -> runtime::RuntimeBuilder {
    runtime::builder()
        .server_policy(policy)
        .header_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(1))
}

fn policy(route: HealthRoute) -> ServerPolicy {
    ServerPolicy::default().health_route(route).unwrap()
}

/// A router that answers `path` itself.
fn owning(path: &str) -> Router {
    let mut router = Router::new();
    router.get(path, |_req: &Request| async {
        Response::text(200, SERVICE_ANSWER)
    });
    router
}

fn get(addr: std::net::SocketAddr, path: &str) -> (u16, Box<str>) {
    let response = common::send(addr, "GET", path, &[], b"");
    (
        response.status,
        String::from_utf8_lossy(&response.body)
            .into_owned()
            .into_boxed_str(),
    )
}

fn assert_report(addr: std::net::SocketAddr, path: &str) {
    let (status, body) = get(addr, path);
    assert_eq!(status, 200, "{path} did not answer the health report");
    assert!(
        body.contains(r#""status":"healthy""#),
        "{path} answered {body} instead of the health report"
    );
}

fn assert_service(addr: std::net::SocketAddr, path: &str) {
    assert_eq!(
        get(addr, path),
        (200, Box::from(SERVICE_ANSWER)),
        "{path} did not reach the service's own route"
    );
}

fn assert_unrouted(addr: std::net::SocketAddr, path: &str) {
    assert_eq!(get(addr, path).0, 404, "{path} answered on this server");
}

/// Freeze one server inside the current runtime and return its refusal.
fn refusal(serve: impl FnOnce(camber::net::Listener) -> Result<(), RuntimeError>) -> Box<str> {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    match serve(listener) {
        Err(RuntimeError::InvalidArgument(message)) => message,
        other => panic!("expected the server to refuse its routes, got {other:?}"),
    }
}

fn assert_names(message: &str, path: &str) {
    assert!(
        message.contains(path),
        "the refusal did not name {path}: {message}"
    );
}

#[test]
fn disabled_route_hands_health_to_the_router_and_keeps_resource_checks() {
    let log = Arc::new(common::CallbackLog::default());
    common::test_runtime()
        .health_interval(common::TICK)
        .resource(common::ScriptedResource::new("db", &log))
        .run(|| {
            let addr =
                common::spawn_server_with_policy(owning("/health"), policy(HealthRoute::Disabled));
            assert_service(addr, "/health");
            // The readiness pass is the first check; a second one proves the
            // interval kept running with the route off.
            common::wait_until("a periodic health check", || log.count("db", "health") >= 2);
            runtime::request_shutdown();
        })
        .unwrap();
    assert_eq!(
        log.count("db", "shutdown"),
        1,
        "the resource never shut down"
    );
}

#[test]
fn moved_route_serves_the_report_at_its_path_and_frees_health() {
    common::test_runtime()
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let addr = common::spawn_server_with_policy(
                owning("/health"),
                policy(HealthRoute::Path("/camber/health")),
            );
            assert_report(addr, "/camber/health");
            assert_service(addr, "/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn runtime_disabled_route_hides_health_on_a_default_server() {
    runtime_with(policy(HealthRoute::Disabled))
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let addr =
                common::spawn_server_with_policy(Router::new(), policy(HealthRoute::Default));
            assert_unrouted(addr, "/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn server_disabled_route_wins_over_a_runtime_path() {
    runtime_with(policy(HealthRoute::Path("/ops/health")))
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let addr =
                common::spawn_server_with_policy(Router::new(), policy(HealthRoute::Disabled));
            assert_unrouted(addr, "/ops/health");
            assert_unrouted(addr, "/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn default_server_inherits_the_runtime_path() {
    runtime_with(policy(HealthRoute::Path("/ops/health")))
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let addr = common::spawn_server(Router::new());
            assert_report(addr, "/ops/health");
            assert_unrouted(addr, "/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn server_path_wins_over_the_runtime_path() {
    runtime_with(policy(HealthRoute::Path("/ops/health")))
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let addr = common::spawn_server_with_policy(
                Router::new(),
                policy(HealthRoute::Path("/camber/health")),
            );
            assert_report(addr, "/camber/health");
            assert_unrouted(addr, "/ops/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn router_health_beside_a_live_route_is_refused() {
    common::test_runtime()
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let message =
                refusal(|listener| http::server(owning("/health")).serve_listener(listener));
            assert_names(&message, "/health");

            // The trie matches `//health/` as `/health`, so that spelling
            // shadows the same request.
            let message =
                refusal(|listener| http::server(owning("//health/")).serve_listener(listener));
            assert_names(&message, "/health");

            let message = refusal(|listener| {
                http::server(owning("/camber/health"))
                    .policy(policy(HealthRoute::Path("/camber/health")))
                    .serve_listener(listener)
            });
            assert_names(&message, "/camber/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_router_health_beside_a_live_route_is_refused() {
    common::test_runtime()
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let mut hosts = HostRouter::new();
            hosts.add("api.example", Router::new());
            hosts.add("admin.example", owning("/health"));
            let message = refusal(|listener| http::server_hosts(hosts).serve_listener(listener));
            assert_names(&message, "/health");

            let mut hosts = HostRouter::new();
            hosts.add("api.example", Router::new());
            hosts.set_default(owning("/health"));
            let message = refusal(|listener| http::server_hosts(hosts).serve_listener(listener));
            assert_names(&message, "/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn router_metrics_beside_the_live_metrics_route_is_refused() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let message =
                refusal(|listener| http::server(owning("/metrics")).serve_listener(listener));
            assert_names(&message, "/metrics");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn router_health_without_resources_is_served_by_the_router() {
    common::test_runtime()
        .run(|| {
            let addr = common::spawn_server(owning("/health"));
            assert_service(addr, "/health");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn router_health_on_a_standalone_server_is_served_by_the_router() {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let timeout = Duration::from_secs(5);
    let server = tokio
        .block_on(async { common::spawn_server_ready(owning("/health"), timeout) })
        .unwrap();
    assert_service(server.local_addr(), "/health");
    tokio
        .block_on(async { server.shutdown_bounded(timeout) })
        .unwrap();
}

#[test]
fn parameter_route_still_yields_to_the_live_route() {
    common::test_runtime()
        .resource(common::ScriptedResource::new(
            "db",
            &Arc::new(common::CallbackLog::default()),
        ))
        .run(|| {
            let addr = common::spawn_server(owning("/:page"));
            assert_report(addr, "/health");
            assert_service(addr, "/about");
            runtime::request_shutdown();
        })
        .unwrap();
}
