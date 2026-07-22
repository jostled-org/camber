use std::time::Duration;

use crate::support::FixtureError;

const FIXTURE_CHILD: &str = "fixtures::fixture_child_waits";

#[test]
fn fixture_child_waits() -> Result<(), FixtureError> {
    match std::env::var("CAMBER_BENCH_FIXTURE_CHILD").as_deref() {
        Ok("wait") => std::thread::park_timeout(Duration::from_secs(30)),
        Ok("output") => {
            use std::io::Write;
            let bytes = vec![b'x'; crate::support::process::CAPTURE_LIMIT + 4096];
            std::io::stdout().write_all(&bytes)?;
            std::io::stderr().write_all(&bytes)?;
        }
        Ok("ready") => {
            println!("not-ready");
            println!("ready ");
            println!("ready");
        }
        Ok("stderr-ready") => {
            eprintln!("not-ready");
            eprintln!("ready ");
            eprintln!("ready");
        }
        Ok("address") => {
            use std::io::Write;

            let server = crate::support::server::OwnedHttpServer::ok()?;
            println!("{}", "x".repeat(5000));
            println!("{}", server.addr());
            std::io::stdout().flush()?;
            std::thread::park_timeout(Duration::from_secs(30));
            drop(server);
        }
        _ => {}
    }
    Ok(())
}

#[test]
fn child_guard_waits_for_success() -> Result<(), FixtureError> {
    let mut child = crate::support::process::spawn_current_test(FIXTURE_CHILD)?;
    let status = child.wait(Duration::from_secs(2))?;
    assert!(status.success());
    Ok(())
}

#[test]
fn child_guard_reaps_after_assertion_panic() -> Result<(), FixtureError> {
    let child = crate::support::process::spawn_current_test_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "wait",
    )?;
    let pid = child.id();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let child_guard = child;
        assert_eq!(child_guard.id(), 0, "exercise assertion-panic cleanup");
    }));
    assert!(panic.is_err());
    assert!(!crate::support::process::process_exists(pid));
    Ok(())
}

#[test]
fn child_guard_kills_and_waits_after_readiness_timeout() -> Result<(), FixtureError> {
    let mut child = crate::support::process::spawn_current_test_ready_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "wait",
        "ready",
    )?;
    let pid = child.id();
    let error = match child.wait_for_ready(Duration::from_millis(100)) {
        Ok(()) => return Err(FixtureError::new("child unexpectedly became ready")),
        Err(error) => error,
    };
    assert!(error.to_string().contains("readiness timed out"));
    assert!(!crate::support::process::process_exists(pid));
    Ok(())
}

#[test]
fn child_guard_signals_only_exact_ready_line() -> Result<(), FixtureError> {
    let mut marker_child = crate::support::process::spawn_current_test_ready_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "ready",
        "ready",
    )?;
    marker_child.wait_for_ready(Duration::from_secs(1))?;
    assert!(marker_child.wait(Duration::from_secs(1))?.success());

    let mut child = crate::support::address_process::AddressChild::spawn_current_test(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "address",
        false,
    )?;
    let addr = child.wait_for_address(Duration::from_secs(1))?;
    let response = crate::support::http::get(addr, "/", Duration::from_secs(1))?;
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_ref(), b"ok");
    child.terminate()?;
    Ok(())
}

#[test]
fn child_guard_accepts_exact_ready_line_from_stderr() -> Result<(), FixtureError> {
    let mut child = crate::support::process::spawn_current_test_ready_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "stderr-ready",
        "ready",
    )?;
    child.wait_for_ready(Duration::from_secs(1))?;
    assert!(child.wait(Duration::from_secs(1))?.success());
    Ok(())
}

#[test]
fn child_guard_kills_and_reaps_after_wait_timeout() -> Result<(), FixtureError> {
    let mut child = crate::support::process::spawn_current_test_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "wait",
    )?;
    let pid = child.id();
    let error = match child.wait(Duration::from_millis(100)) {
        Ok(_) => {
            return Err(FixtureError::new(
                "child completed before its wait deadline",
            ));
        }
        Err(error) => error,
    };
    assert!(error.to_string().contains("child wait timed out"));
    assert!(!crate::support::process::process_exists(pid));
    Ok(())
}

#[test]
fn child_guard_reaps_when_wait_deadline_overflows() -> Result<(), FixtureError> {
    let mut child = crate::support::process::spawn_current_test_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "wait",
    )?;
    let pid = child.id();
    let error = match child.wait(Duration::MAX) {
        Ok(_) => return Err(FixtureError::new("overflowing wait deadline was accepted")),
        Err(error) => error,
    };
    assert!(error.to_string().contains("deadline overflow"));
    assert!(!crate::support::process::process_exists(pid));
    Ok(())
}

#[test]
fn child_output_is_drained_and_capped() -> Result<(), FixtureError> {
    let mut child = crate::support::process::spawn_current_test_child(
        FIXTURE_CHILD,
        "CAMBER_BENCH_FIXTURE_CHILD",
        "output",
    )?;
    let status = child.wait(Duration::from_secs(2))?;
    let output = child.captured_output()?;
    assert!(status.success());
    assert_eq!(output.stdout.len(), crate::support::process::CAPTURE_LIMIT);
    assert_eq!(output.stderr.len(), crate::support::process::CAPTURE_LIMIT);
    assert!(output.stdout_truncated);
    assert!(output.stderr_truncated);
    Ok(())
}

#[test]
fn bounded_read_reports_timeout() -> Result<(), FixtureError> {
    let server = crate::support::server::OwnedHttpServer::stall_after_headers()?;
    let error = match crate::support::http::get(server.addr(), "/", Duration::from_millis(500)) {
        Ok(_) => return Err(FixtureError::new("incomplete response did not time out")),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("response read timed out"),
        "unexpected bounded-read error: {error}"
    );
    Ok(())
}

#[test]
fn bounded_read_rejects_oversized_response() -> Result<(), FixtureError> {
    let server = crate::support::server::OwnedHttpServer::oversized_response()?;
    let error = match crate::support::http::get(server.addr(), "/", Duration::from_secs(2)) {
        Ok(_) => return Err(FixtureError::new("oversized response was accepted")),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("exceeded 1 MiB limit"),
        "unexpected oversized-response error: {error}"
    );
    Ok(())
}

#[test]
fn bounded_read_rejects_overflowing_content_length() -> Result<(), FixtureError> {
    let server = crate::support::server::OwnedHttpServer::overflowing_content_length()?;
    let error = match crate::support::http::get(server.addr(), "/", Duration::from_secs(1)) {
        Ok(_) => return Err(FixtureError::new("overflowing response frame was accepted")),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("HTTP body length overflow"),
        "unexpected content-length error: {error}"
    );
    Ok(())
}

#[test]
fn owned_server_accepts_headers_across_read_polls() -> Result<(), FixtureError> {
    use std::io::{Read, Write};

    let server = crate::support::server::OwnedHttpServer::ok()?;
    let mut stream = std::net::TcpStream::connect(server.addr())?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost")?;
    std::thread::sleep(Duration::from_millis(300));
    stream.write_all(b": localhost\r\nConnection: close\r\n\r\n")?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    Ok(())
}

#[test]
fn owned_server_join_completes() -> Result<(), FixtureError> {
    let mut server = crate::support::server::OwnedHttpServer::ok()?;
    let response = crate::support::http::get(server.addr(), "/", Duration::from_secs(1))?;
    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_ref(), b"ok");
    server.shutdown(Duration::from_secs(1))?;
    assert!(server.is_joined());
    Ok(())
}

#[test]
fn owned_server_reaps_after_assertion_panic() -> Result<(), FixtureError> {
    let server = crate::support::server::OwnedHttpServer::ok()?;
    let cleanup = server.cleanup_witness();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let server_guard = server;
        assert_eq!(server_guard.addr().port(), 0, "exercise server cleanup");
    }));
    assert!(panic.is_err());
    assert!(
        cleanup.load(std::sync::atomic::Ordering::Acquire),
        "owned server cleanup did not complete after assertion panic"
    );
    Ok(())
}

#[test]
fn owned_server_worker_panic_is_joined() -> Result<(), FixtureError> {
    let mut server = crate::support::server::OwnedHttpServer::panic_after_ready()?;
    let error = match server.shutdown(Duration::from_secs(1)) {
        Ok(()) => return Err(FixtureError::new("worker panic was not reported")),
        Err(error) => error,
    };
    assert!(error.to_string().contains("thread panicked"));
    assert!(server.is_joined());
    Ok(())
}

#[test]
fn owned_server_readiness_failure_is_joined() -> Result<(), FixtureError> {
    let error = match crate::support::server::OwnedHttpServer::readiness_failure() {
        Ok(_) => return Err(FixtureError::new("missing readiness signal was accepted")),
        Err(error) => error,
    };
    assert!(error.to_string().contains("readiness failed"));
    Ok(())
}
