use std::net::SocketAddr;
use std::time::Duration;

use crate::support::FixtureError;

const BENCH_CAMBER: &str = env!("CARGO_BIN_EXE_bench-camber");
const BENCH_AXUM: &str = env!("CARGO_BIN_EXE_bench-axum");
const BENCH_UPSTREAM: &str = env!("CARGO_BIN_EXE_bench-upstream");
const ISOLATED_SERVER_CONTRACT: &str = "CAMBER_BENCH_ISOLATED_SERVER_CONTRACT";

fn run_isolated(
    test_name: &str,
    contract: impl FnOnce() -> Result<(), FixtureError>,
) -> Result<(), FixtureError> {
    match std::env::var(ISOLATED_SERVER_CONTRACT).as_deref() {
        Ok(selected) if selected == test_name => contract(),
        _ => {
            let mut child = crate::support::process::spawn_current_test_child(
                test_name,
                ISOLATED_SERVER_CONTRACT,
                test_name,
            )?;
            let status = child.wait(Duration::from_secs(10))?;
            let output = child.captured_output()?;
            assert!(
                status.success(),
                "isolated server contract failed: stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(())
        }
    }
}

fn spawn_server(
    binary: &str,
    args: &[&str],
) -> Result<crate::support::process::ServerProcess, FixtureError> {
    let server = crate::support::process::ServerProcess::spawn(binary, args)
        .map_err(|error| FixtureError::new(format!("failed to spawn {binary}: {error}")))?;
    assert_ne!(server.id(), 0, "spawned server must have a process id");
    Ok(server)
}

fn http_get(url: &str) -> Result<(u16, String), FixtureError> {
    let stripped = url.strip_prefix("http://").unwrap_or(url);
    let (addr_str, path) = match stripped.find('/') {
        Some(index) => (&stripped[..index], &stripped[index..]),
        None => (stripped, "/"),
    };
    let addr = addr_str
        .parse()
        .map_err(|error| FixtureError::new(format!("invalid fixture address: {error}")))?;
    let response = crate::support::http::get(addr, path, Duration::from_secs(5))?;
    Ok((
        response.status,
        String::from_utf8(response.body.into_vec())?,
    ))
}

#[test]
fn camber_hello_text_server_responds_200() -> Result<(), FixtureError> {
    run_isolated("servers::camber_hello_text_server_responds_200", || {
        let (addr, server_handle) = camber_bench::servers::camber_server::start_hello_text()?;
        let (status, body) = http_get(&format!("http://{addr}/"))?;
        assert_eq!(status, 200);
        assert_eq!(body, "Hello, world!");
        drop(server_handle);
        Ok(())
    })
}

#[test]
fn axum_hello_text_server_responds_200() -> Result<(), FixtureError> {
    run_isolated("servers::axum_hello_text_server_responds_200", || {
        let (addr, server_handle) = camber_bench::servers::axum_server::start_hello_text()?;
        let (status, body) = http_get(&format!("http://{addr}/"))?;
        assert_eq!(status, 200);
        assert_eq!(body, "Hello, world!");
        drop(server_handle);
        Ok(())
    })
}

#[test]
fn camber_path_param_returns_extracted_id() -> Result<(), FixtureError> {
    run_isolated("servers::camber_path_param_returns_extracted_id", || {
        let (addr, server_handle) = camber_bench::servers::camber_server::start_path_param()?;
        let (status, body) = http_get(&format!("http://{addr}/users/42"))?;
        assert_eq!(status, 200);
        assert!(body.contains("42"), "response should contain extracted id");
        drop(server_handle);
        Ok(())
    })
}

#[test]
fn camber_db_query_returns_json_after_simulated_latency() -> Result<(), FixtureError> {
    run_isolated(
        "servers::camber_db_query_returns_json_after_simulated_latency",
        || {
            let upstream = camber_bench::servers::upstream::start()?;
            let (addr, server_handle) =
                camber_bench::servers::camber_server::start_db_query(upstream)?;
            let start = std::time::Instant::now();
            let (status, body) = http_get(&format!("http://{addr}/query"))?;
            let elapsed = start.elapsed();
            assert_eq!(status, 200);
            assert!(
                body.contains("\"id\""),
                "response should be JSON with id field"
            );
            assert!(
                elapsed >= Duration::from_millis(1),
                "expected >= 1ms latency from simulated db, got {elapsed:?}"
            );
            drop(server_handle);
            Ok(())
        },
    )
}

#[test]
fn camber_middleware_stack_applies_all_middleware() -> Result<(), FixtureError> {
    run_isolated(
        "servers::camber_middleware_stack_applies_all_middleware",
        || {
            let upstream = camber_bench::servers::upstream::start()?;
            let (addr, server_handle) =
                camber_bench::servers::camber_server::start_middleware_stack(upstream)?;
            let response = crate::support::http::get_with_headers(
                addr,
                "/",
                &[("Origin", "http://example.com")],
                Duration::from_secs(5),
            )?;
            assert!(
                response.header("access-control-allow-origin").is_some(),
                "response should have CORS headers, got: {response:?}"
            );
            assert_eq!(response.status, 200);
            drop(server_handle);
            Ok(())
        },
    )
}

#[test]
fn camber_db_query_returns_json_via_fan_out_module() -> Result<(), FixtureError> {
    run_isolated(
        "servers::camber_db_query_returns_json_via_fan_out_module",
        || {
            let upstream = camber_bench::servers::upstream::start()?;
            let (addr, server_handle) =
                camber_bench::servers::camber_server::start_db_query(upstream)?;
            let (status, body) = http_get(&format!("http://{addr}/query"))?;
            assert_eq!(status, 200);
            assert!(
                body.contains("\"id\""),
                "response should be JSON with id field"
            );
            drop(server_handle);
            Ok(())
        },
    )
}

#[test]
fn bench_camber_hello_text_responds_200() -> Result<(), FixtureError> {
    let server = spawn_server(BENCH_CAMBER, &["--bench", "hello_text"])?;
    let (status, body) = http_get(&format!("http://127.0.0.1:{}/", server.port))?;
    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
    Ok(())
}

#[test]
fn bench_axum_hello_text_responds_200() -> Result<(), FixtureError> {
    let server = spawn_server(BENCH_AXUM, &["--bench", "hello_text"])?;
    let (status, body) = http_get(&format!("http://127.0.0.1:{}/", server.port))?;
    assert_eq!(status, 200);
    assert_eq!(body, "Hello, world!");
    Ok(())
}

#[test]
fn bench_upstream_responds_with_delay() -> Result<(), FixtureError> {
    let server = spawn_server(BENCH_UPSTREAM, &[])?;
    let start = std::time::Instant::now();
    let addr = SocketAddr::from(([127, 0, 0, 1], server.port));
    let response = crate::support::http::get(addr, "/", Duration::from_secs(5))?;
    let elapsed = start.elapsed();
    assert_eq!(response.status, 200);
    assert!(
        elapsed >= Duration::from_millis(1),
        "expected >= 1ms latency, got {elapsed:?}"
    );
    Ok(())
}

#[test]
fn bench_camber_db_query_hits_upstream() -> Result<(), FixtureError> {
    let upstream = spawn_server(BENCH_UPSTREAM, &[])?;
    let upstream_addr = format!("127.0.0.1:{}", upstream.port);
    let server = spawn_server(
        BENCH_CAMBER,
        &["--bench", "db_query", "--upstream", &upstream_addr],
    )?;
    let (status, body) = http_get(&format!("http://127.0.0.1:{}/query", server.port))?;
    assert_eq!(status, 200);
    assert!(
        body.contains("\"id\""),
        "response should be JSON with id field, got: {body}"
    );
    Ok(())
}
