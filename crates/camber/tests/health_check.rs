mod common;

use camber::http::{self, Next, ProxyHealthResource, Request, Response, Router};
use camber::{Resource, RuntimeError, runtime};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

struct HealthyResource(&'static str);

impl Resource for HealthyResource {
    fn name(&self) -> &str {
        self.0
    }
    fn health_check(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

struct UnhealthyResource(&'static str);

impl Resource for UnhealthyResource {
    fn name(&self) -> &str {
        self.0
    }
    fn health_check(&self) -> Result<(), RuntimeError> {
        Err(RuntimeError::InvalidArgument("connection refused".into()))
    }
    fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn auth_middleware(req: &Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let has_auth = req
        .headers()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
    match has_auth {
        true => next.call(req),
        false => Box::pin(async { Response::text(401, "unauthorized").expect("valid status") }),
    }
}

#[camber::test]
async fn proxy_returns_503_when_backend_unhealthy() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "from-backend")
    });
    let backend_addr = common::spawn_server(backend);

    let healthy = Arc::new(AtomicBool::new(true));

    let mut main = Router::new();
    main.proxy_checked("", &format!("http://{backend_addr}"), Arc::clone(&healthy));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "from-backend");

    healthy.store(false, Ordering::Relaxed);

    let resp = http::get(&format!("http://{main_addr}/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_recovers_when_backend_returns() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "alive")
    });
    let backend_addr = common::spawn_server(backend);

    let healthy = Arc::new(AtomicBool::new(true));

    let mut main = Router::new();
    main.proxy_checked("", &format!("http://{backend_addr}"), Arc::clone(&healthy));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    healthy.store(false, Ordering::Relaxed);
    let resp = http::get(&format!("http://{main_addr}/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);

    healthy.store(true, Ordering::Relaxed);
    let resp = http::get(&format!("http://{main_addr}/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "alive");

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_without_health_check_unchanged() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "proxied")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/api/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "proxied");

    let mut broken = Router::new();
    broken.proxy("/api", "http://127.0.0.1:1");
    let broken_addr = common::spawn_server(broken);

    let resp = http::get(&format!("http://{broken_addr}/api/anything"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);

    runtime::request_shutdown();
}

#[test]
fn proxy_health_checker_reports_via_health_endpoint() {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_addr = std_listener.local_addr().unwrap();
    let backend_url = format!("http://{backend_addr}");
    drop(std_listener);

    let resource = ProxyHealthResource::new(&backend_url, "/up");
    let healthy = resource.routing_flag();

    runtime::builder()
        .shutdown_timeout(Duration::from_secs(3))
        .health_interval(Duration::from_secs(1))
        .resource(resource)
        .run(|| {
            let mut backend = Router::new();
            backend.get("/up", |_req: &Request| async { Response::text(200, "ok") });
            backend.get("/hello", |_req: &Request| async {
                Response::text(200, "proxied")
            });
            let backend_listener = camber::net::listen(&format!("{backend_addr}")).unwrap();
            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                http::serve_listener(backend_listener, backend)
            });

            let mut main = Router::new();
            main.proxy_checked("", &format!("http://{backend_addr}"), healthy);
            let main_addr = common::spawn_server(main);

            std::thread::sleep(Duration::from_millis(1500));

            let resp = common::block_on(http::get(&format!("http://{main_addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);
            let body = resp.body();
            assert!(
                body.contains(&format!("http://{backend_addr}")),
                "expected backend URL in health response: {body}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn proxy_health_checker_still_controls_routing() {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_addr = std_listener.local_addr().unwrap();
    let backend_url = format!("http://{backend_addr}");
    drop(std_listener);

    let resource = ProxyHealthResource::new(&backend_url, "/up");
    let healthy = resource.routing_flag();

    runtime::builder()
        .shutdown_timeout(Duration::from_secs(5))
        .health_interval(Duration::from_secs(1))
        .resource(resource)
        .run(|| {
            let mut backend = Router::new();
            backend.get("/up", |_req: &Request| async {
                Response::text(500, "down")
            });
            backend.get("/hello", |_req: &Request| async {
                Response::text(200, "from-backend")
            });
            let backend_listener = camber::net::listen(&format!("{backend_addr}")).unwrap();
            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                http::serve_listener(backend_listener, backend)
            });

            let mut main = Router::new();
            main.proxy_checked("", &format!("http://{backend_addr}"), Arc::clone(&healthy));
            let main_addr = common::spawn_server(main);

            std::thread::sleep(Duration::from_millis(1500));

            let resp = common::block_on(http::get(&format!("http://{main_addr}/hello"))).unwrap();
            assert_eq!(resp.status(), 503);

            healthy.store(true, Ordering::Release);

            let resp = common::block_on(http::get(&format!("http://{main_addr}/hello"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "from-backend");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn health_endpoint_returns_200_when_all_resources_healthy() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .resource(HealthyResource("cache"))
        .run(|| {
            let addr = common::spawn_server(Router::new());
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert!(resp.body().contains(r#""status":"healthy""#));
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn health_endpoint_returns_503_when_any_resource_unhealthy() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .resource(UnhealthyResource("cache"))
        .run(|| {
            let addr = common::spawn_server(Router::new());
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 503);
            assert!(resp.body().contains(r#""status":"unhealthy""#));
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn health_check_runs_on_configured_interval() {
    let count = Arc::new(AtomicUsize::new(0));

    struct CountingResource(Arc<AtomicUsize>);

    impl Resource for CountingResource {
        fn name(&self) -> &str {
            "counter"
        }
        fn health_check(&self) -> Result<(), RuntimeError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn shutdown(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    runtime::builder()
        .shutdown_timeout(Duration::from_secs(5))
        .health_interval(Duration::from_secs(1))
        .resource(CountingResource(Arc::clone(&count)))
        .run(|| {
            std::thread::sleep(Duration::from_millis(2500));
            runtime::request_shutdown();
        })
        .unwrap();

    let calls = count.load(Ordering::Relaxed);
    assert!(
        calls >= 2,
        "health_check should be called at least 2 times, got {calls}"
    );
}

#[test]
fn health_endpoint_lists_individual_resource_status() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .resource(UnhealthyResource("cache"))
        .run(|| {
            let addr = common::spawn_server(Router::new());
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            let body = resp.body();
            assert!(body.contains(r#""db":"ok""#), "expected db:ok in {body}");
            assert!(
                body.contains(r#""cache":"error""#),
                "expected cache:error in {body}"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn health_endpoint_goes_through_middleware() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(auth_middleware);

            let addr = common::spawn_server(router);

            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 401);

            let raw =
                common::raw_request(addr, "GET", "/health", &[("Authorization", "Bearer tok")]);
            assert_eq!(common::status_from_raw(&raw), 200);
            assert!(raw.contains(r#""status":"healthy""#));

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn skip_middleware_for_internal_bypasses_auth() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(auth_middleware);
            let router = router.skip_middleware_for_internal(true);

            let addr = common::spawn_server(router);

            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert!(resp.body().contains(r#""status":"healthy""#));

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn internal_routes_registered_during_freeze() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .with_metrics()
        .run(|| {
            let router = Router::new();
            let addr = common::spawn_server(router);

            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);

            let resp = common::block_on(http::get(&format!("http://{addr}/metrics"))).unwrap();
            assert_eq!(resp.status(), 200);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn spawn_health_checker_initial_probe_sets_unhealthy_flag_before_loop() {
    // Bind and immediately drop so the port is closed — health probe will fail.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    drop(std_listener);

    let healthy = http::spawn_health_checker(
        &format!("http://{addr}"),
        "/health",
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    // The flag must already be false — initial probe ran before the function returned.
    assert!(
        !healthy.load(Ordering::Acquire),
        "flag should be false after initial probe against dead backend"
    );

    runtime::request_shutdown();
}
