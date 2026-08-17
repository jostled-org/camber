#![cfg(feature = "profiling")]

//! The live profiling endpoint, served under the maximum it defaults to.
//!
//! The CPU load a profile is rendered from is the shared fixture owner in
//! `support/service_operation.rs`, because 10.T2 needs the same load over a real
//! peer and two fixtures for one resource are two teardowns that can drift. The
//! bounded-output behavior itself — the frozen maximum, the dropped crossing
//! write, and the operator's typed cause — belongs to 10.T2. What this row keeps
//! is the whole live answer: a real sampling window renders a real SVG, and the
//! unnamed spelling serves it under the documented default.

use crate::common;

use camber::__private::DEFAULT_PROFILING_RESPONSE_LIMIT;
use camber::http::{Request, Response, Router};
use camber::runtime;
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(3);

/// How many busy threads give the sampler stacks to render.
const LOAD_THREADS: usize = 4;

#[test]
fn cpu_load_drop_joins_after_assertion_unwind() {
    let load = common::CpuLoad::start(2);
    let cleanup = load.probe();

    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _load = load;
        assert_eq!(1, 2, "intentional CPU load fixture assertion failure");
    }));

    assert!(unwind.is_err());
    assert_eq!(cleanup.exited(), 2);
    assert_eq!(cleanup.joined(), 2);
    assert_eq!(cleanup.failures(), 0);
}

#[test]
fn profiling_endpoint_returns_flamegraph() {
    // Every load thread reports entry before the capture; no scheduler delay is
    // inferred, and the load is stopped and joined however this row ends.
    let load = common::CpuLoad::start(LOAD_THREADS);
    let ran = common::test_runtime()
        .shutdown_timeout(Duration::from_secs(3))
        .with_profiling()
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
            let addr = server.local_addr();

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
            assert!(
                body.starts_with("<?xml"),
                "expected a rendered SVG document, got: {}",
                &body[..body.len().min(64)],
            );
            // The unnamed spelling serves under the documented maximum, so a
            // rendered answer that arrives at all arrived inside it.
            assert!(
                resp.body_bytes().len() <= DEFAULT_PROFILING_RESPONSE_LIMIT,
                "the rendered profile is retained under the documented maximum",
            );

            server.shutdown_bounded(EVENT_TIMEOUT).unwrap();

            runtime::request_shutdown();
        });
    load.stop();
    ran.unwrap();
}
