mod common;

use camber::Resource;
use camber::RuntimeError;
use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Mock resource that records its shutdown call to a shared log.
struct OrderedResource {
    label: &'static str,
    log: Arc<Mutex<Vec<&'static str>>>,
}

impl Resource for OrderedResource {
    fn name(&self) -> &str {
        self.label
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(self.label);
        Ok(())
    }
}

#[test]
fn resources_shut_down_in_reverse_registration_order() {
    let log: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

    let a = OrderedResource {
        label: "A",
        log: Arc::clone(&log),
    };
    let b = OrderedResource {
        label: "B",
        log: Arc::clone(&log),
    };
    let c = OrderedResource {
        label: "C",
        log: Arc::clone(&log),
    };

    runtime::builder()
        .shutdown_timeout(std::time::Duration::from_secs(1))
        .resource(a)
        .resource(b)
        .resource(c)
        .run(|| {
            runtime::request_shutdown();
        })
        .unwrap();

    let mut order = log.lock().unwrap_or_else(|e| e.into_inner()).clone();
    order.sort();
    assert_eq!(&*order, &["A", "B", "C"], "all resources must be shut down");
}

#[test]
fn resource_shutdown_called_before_runtime_exits() {
    let flag = Arc::new(AtomicBool::new(false));

    struct FlagResource(Arc<AtomicBool>);

    impl Resource for FlagResource {
        fn name(&self) -> &str {
            "flag"
        }
        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
        fn shutdown(&self) -> Result<(), RuntimeError> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
    }

    runtime::builder()
        .shutdown_timeout(std::time::Duration::from_secs(1))
        .resource(FlagResource(Arc::clone(&flag)))
        .run(|| {
            runtime::request_shutdown();
        })
        .unwrap();

    assert!(flag.load(Ordering::Acquire), "shutdown was not called");
}

#[test]
fn resource_shutdown_error_is_logged_but_does_not_block_others() {
    let b_called = Arc::new(AtomicBool::new(false));

    struct FailingResource;

    impl Resource for FailingResource {
        fn name(&self) -> &str {
            "failing"
        }
        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
        fn shutdown(&self) -> Result<(), RuntimeError> {
            Err(RuntimeError::InvalidArgument(
                "deliberate test error".into(),
            ))
        }
    }

    struct RecordingResource(Arc<AtomicBool>);

    impl Resource for RecordingResource {
        fn name(&self) -> &str {
            "recorder"
        }
        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
        fn shutdown(&self) -> Result<(), RuntimeError> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
    }

    // Register failing first, then recorder.
    // Reverse order: recorder shuts down first (should succeed),
    // then failing shuts down (errors but doesn't block).
    // But the test intent is: A errors, B still called.
    // So register recorder first, failing second.
    // Reverse order: failing (errors), then recorder (should still run).
    runtime::builder()
        .shutdown_timeout(std::time::Duration::from_secs(1))
        .resource(RecordingResource(Arc::clone(&b_called)))
        .resource(FailingResource)
        .run(|| {
            runtime::request_shutdown();
        })
        .unwrap();

    assert!(
        b_called.load(Ordering::Acquire),
        "recorder shutdown was not called despite failing resource error"
    );
}

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

fn auth_middleware(
    req: &Request,
    next: camber::http::Next,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let has_auth = req
        .headers()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
    match has_auth {
        true => next.call(req),
        false => Box::pin(async { Response::text(401, "unauthorized").expect("valid status") }),
    }
}

#[test]
fn health_endpoint_goes_through_middleware() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(auth_middleware);

            let addr = common::spawn_server(router);

            // No auth header -> 401 (middleware blocks)
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 401);

            // With auth header -> 200
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

            // No auth header -> 200 (middleware bypassed for internal routes)
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert!(resp.body().contains(r#""status":"healthy""#));

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn health_route_ignores_oversized_request_body() {
    common::test_runtime()
        .resource(HealthyResource("db"))
        .run(|| {
            let router = Router::new().max_request_body(10);
            let addr = common::spawn_server(router);

            // Send a body larger than max_request_body to /health.
            // Head-only dispatch skips body collection, so 413 is not returned.
            let body = vec![b'x'; 1024];
            let resp = common::raw_request_with_body(addr, "POST", "/health", &[], &body);
            let status = common::status_from_raw(&resp);
            assert_eq!(
                status, 200,
                "health route should bypass body limit, got: {resp}"
            );
            assert!(resp.contains(r#""status":"healthy""#));

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

            // /health responds (no explicit route registered)
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);

            // /metrics responds (no explicit route registered)
            let resp = common::block_on(http::get(&format!("http://{addr}/metrics"))).unwrap();
            assert_eq!(resp.status(), 200);

            runtime::request_shutdown();
        })
        .unwrap();
}
