#![cfg(feature = "profiling")]

use crate::common;

use camber::http::{Request, Response, Router};
use camber::runtime;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(3);

struct CpuWorkers {
    running: Arc<AtomicBool>,
    handles: Vec<Option<std::thread::JoinHandle<()>>>,
    cleanup: Arc<CpuWorkersCleanupState>,
}

#[derive(Default)]
struct CpuWorkersCleanupState {
    exited: AtomicUsize,
    joined: AtomicUsize,
    cleanup_errors: AtomicUsize,
}

struct CpuWorkersCleanupProbe {
    state: Arc<CpuWorkersCleanupState>,
}

impl CpuWorkersCleanupProbe {
    fn exited(&self) -> usize {
        self.state.exited.load(Ordering::Acquire)
    }

    fn joined(&self) -> usize {
        self.state.joined.load(Ordering::Acquire)
    }

    fn cleanup_errors(&self) -> usize {
        self.state.cleanup_errors.load(Ordering::Acquire)
    }
}

impl CpuWorkers {
    fn start(count: usize) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let cleanup = Arc::new(CpuWorkersCleanupState::default());
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(count);
        let handles = (0..count)
            .map(|_| {
                let running = Arc::clone(&running);
                let cleanup = Arc::clone(&cleanup);
                let started_tx = started_tx.clone();
                Some(std::thread::spawn(move || {
                    started_tx.send(()).unwrap();
                    let mut value = 0u64;
                    while running.load(Ordering::Relaxed) {
                        for _ in 0..10_000 {
                            value = value
                                .wrapping_mul(6_364_136_223_846_793_005)
                                .wrapping_add(1);
                        }
                    }
                    std::hint::black_box(value);
                    cleanup.exited.fetch_add(1, Ordering::Release);
                }))
            })
            .collect();
        drop(started_tx);
        let workers = Self {
            running,
            handles,
            cleanup,
        };
        for _ in 0..count {
            started_rx.recv_timeout(EVENT_TIMEOUT).unwrap();
        }
        workers
    }

    fn stop(mut self) {
        let cleanup_errors = self.shutdown();
        assert_eq!(cleanup_errors, 0, "CPU worker cleanup failed");
    }

    fn cleanup_probe(&self) -> CpuWorkersCleanupProbe {
        CpuWorkersCleanupProbe {
            state: Arc::clone(&self.cleanup),
        }
    }

    fn shutdown(&mut self) -> usize {
        self.running.store(false, Ordering::Release);
        self.handles
            .iter_mut()
            .filter(|handle| handle.is_some())
            .map(
                |handle| match common::join_thread_bounded(handle, EVENT_TIMEOUT) {
                    Ok(()) => {
                        self.cleanup.joined.fetch_add(1, Ordering::Release);
                        0
                    }
                    Err(_) => {
                        self.cleanup.cleanup_errors.fetch_add(1, Ordering::Release);
                        1
                    }
                },
            )
            .sum()
    }
}

impl Drop for CpuWorkers {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[test]
fn cpu_workers_drop_joins_after_assertion_unwind() {
    let workers = CpuWorkers::start(2);
    let cleanup = workers.cleanup_probe();

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _workers = workers;
        assert_eq!(1, 2, "intentional CPU worker fixture assertion failure");
    }));

    assert!(unwind.is_err());
    assert_eq!(cleanup.exited(), 2);
    assert_eq!(cleanup.joined(), 2);
    assert_eq!(cleanup.cleanup_errors(), 0);
}

#[test]
fn profiling_endpoint_returns_flamegraph() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(3))
        .with_profiling()
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
            let addr = server.local_addr();

            // Every worker reports entry before capture; no scheduler delay is inferred.
            let workers = CpuWorkers::start(4);

            // Request a 1-second CPU profile
            let resp = common::block_on(camber::http::get(&format!(
                "http://{addr}/debug/pprof/cpu?seconds=1"
            )))
            .unwrap();
            assert_eq!(resp.status(), 200);

            let body = resp.body();
            assert!(
                !body.is_empty(),
                "expected non-empty flamegraph SVG, got empty body"
            );

            workers.stop();
            server.shutdown_bounded(EVENT_TIMEOUT).unwrap();

            runtime::request_shutdown();
        })
        .unwrap();
}
