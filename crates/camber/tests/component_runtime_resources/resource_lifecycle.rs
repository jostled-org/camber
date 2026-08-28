use crate::common;

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

    let order: Box<[&str]> = log
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .into_boxed_slice();
    assert_eq!(
        &*order,
        &["C", "B", "A"],
        "teardown must visit resources in reverse registration order"
    );
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

    // Registered recorder first, failing second, so teardown's reverse order
    // reaches the failing callback before the one that must still run.
    let outcome = runtime::builder()
        .shutdown_timeout(std::time::Duration::from_secs(1))
        .resource(RecordingResource(Arc::clone(&b_called)))
        .resource(FailingResource)
        .run(|| {
            runtime::request_shutdown();
        });

    assert!(
        b_called.load(Ordering::Acquire),
        "recorder shutdown was not called despite failing resource error"
    );
    // The error is no longer disposed of in a log line: it reaches the caller
    // through the aggregate, named against the resource that returned it.
    let error = outcome.expect_err("a returned teardown error did not reach the caller");
    assert!(
        matches!(&error, RuntimeError::Lifecycle(failures) if failures.len() == 1),
        "one returned teardown error must be the whole aggregate: {error:?}"
    );
}

#[test]
fn cancellation_watcher_stops_before_resource_shutdown() {
    struct WatcherProbe(Arc<AtomicBool>);

    impl Resource for WatcherProbe {
        fn name(&self) -> &str {
            "watcher-probe"
        }

        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn shutdown(&self) -> Result<(), RuntimeError> {
            assert!(
                self.0.load(Ordering::Acquire),
                "external cancellation watcher was live during resource shutdown"
            );
            Ok(())
        }
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    let watcher_stopped = Arc::new(AtomicBool::new(false));
    let future_probe = DropProbe(Arc::clone(&watcher_stopped));

    runtime::builder()
        .resource(WatcherProbe(Arc::clone(&watcher_stopped)))
        .run(move || {
            runtime::on_cancel(async move {
                let probe = future_probe;
                std::future::pending::<()>().await;
                drop(probe);
            });
        })
        .unwrap();
}

#[test]
fn resource_shutdown_panic_is_reported_after_other_callbacks_finish() {
    struct PanickingResource;

    impl Resource for PanickingResource {
        fn name(&self) -> &str {
            "panicking"
        }

        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn shutdown(&self) -> Result<(), RuntimeError> {
            panic!("resource shutdown panic");
        }
    }

    struct FinalizationProbe(Arc<AtomicBool>);

    impl Resource for FinalizationProbe {
        fn name(&self) -> &str {
            "finalization-probe"
        }

        fn health_check(&self) -> Result<(), RuntimeError> {
            Ok(())
        }

        fn shutdown(&self) -> Result<(), RuntimeError> {
            self.0.store(true, Ordering::Release);
            Ok(())
        }
    }

    let finalized = Arc::new(AtomicBool::new(false));
    let outcome = runtime::builder()
        .resource(PanickingResource)
        .resource(FinalizationProbe(Arc::clone(&finalized)))
        .run(|| ());

    // Searched for, never elected: the aggregate carries an entry per failed
    // owner in its own rendering order, so a run that recorded a second owner
    // ahead of this one would hand a reader the wrong kind entirely.
    let panicked = match &outcome {
        Err(RuntimeError::Lifecycle(failures)) => failures
            .iter()
            .map(|failure| failure.kind().clone())
            .find(|kind| matches!(kind, camber::LifecycleFailureKind::Resource(_)))
            .unwrap_or_else(|| {
                panic!(
                    "no resource failure reached the aggregate: {:?}",
                    failures
                        .iter()
                        .map(crate::lifecycle_kinds::entry_identity)
                        .collect::<Vec<_>>()
                )
            }),
        other => panic!("resource panic was not reported as an aggregate: {other:?}"),
    };
    assert!(
        matches!(&panicked, camber::LifecycleFailureKind::Resource(resource)
            if matches!(resource.kind(), camber::ResourceFailureKind::Panicked(message)
                if &**message == "resource shutdown panic")),
        "the aggregate did not carry the callback's own panic payload: {panicked}"
    );
    assert!(
        finalized.load(Ordering::Acquire),
        "a panicking callback skipped another resource's finalization"
    );
    assert!(
        runtime::run(|| ()).is_ok(),
        "runtime context was not restored"
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
fn health_check_runs_on_configured_interval() {
    let (checked_tx, checked_rx) = std::sync::mpsc::sync_channel(1);

    struct CountingResource {
        count: AtomicUsize,
        checked: std::sync::mpsc::SyncSender<usize>,
    }

    impl Resource for CountingResource {
        fn name(&self) -> &str {
            "counter"
        }
        fn health_check(&self) -> Result<(), RuntimeError> {
            let check = self.count.fetch_add(1, Ordering::Relaxed) + 1;
            self.checked.send(check).unwrap();
            Ok(())
        }
        fn shutdown(&self) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    runtime::builder()
        .shutdown_timeout(Duration::from_secs(5))
        .health_interval(Duration::from_secs(1))
        .resource(CountingResource {
            count: AtomicUsize::new(0),
            checked: checked_tx,
        })
        .run(|| {
            let initial = checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let periodic = checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!((initial, periodic), (1, 2));
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
