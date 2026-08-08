use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::support::FixtureError;
use crate::support::http::{Backend, BackendError, BackendShutdown, read_response, request_tcp};
use crate::support::process::{CHILD_EXIT_TIMEOUT, ChildGuard, ReadinessTarget, TerminationKind};

#[test]
fn child_guard_reaps_natural_exit_between_observation_and_termination() -> Result<(), FixtureError>
{
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "read line"]).stdin(Stdio::piped());
    let mut child = ChildGuard::spawn_command(command)?;
    let child_id = child.id();
    let reap_probe = child
        .take_reap_probe()
        .ok_or_else(|| FixtureError::new("synchronized child reap probe was absent"))?;
    let termination = child.shutdown_after_observation(|observed_child| {
        let mut stdin = observed_child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("synchronized child stdin was not piped"))?;
        stdin.write_all(b"exit\n")
    })?;
    let reaped = reap_probe.wait()?;
    assert_eq!(termination, TerminationKind::NaturalExitAfterObservation);
    assert_eq!(reaped.child_id(), child_id);
    assert!(
        reaped.status().success(),
        "child was killed instead of exiting"
    );
    Ok(())
}

#[test]
fn child_guard_reports_natural_reap_status() -> Result<(), FixtureError> {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exit 0"]);
    let mut child = ChildGuard::spawn_command(command)?;
    let child_id = child.id();
    let reap_probe = child
        .take_reap_probe()
        .ok_or_else(|| FixtureError::new("successful child reap probe was absent"))?;
    let output = child.wait_with_output(CHILD_EXIT_TIMEOUT)?;
    let reaped = reap_probe.wait()?;
    assert!(output.status.success());
    assert_eq!(reaped.child_id(), child_id);
    assert!(reaped.status().success());
    Ok(())
}

#[test]
fn child_guard_timeout_kills_and_reports_reaped_status() -> Result<(), FixtureError> {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "while :; do :; done"]);
    let mut child = ChildGuard::spawn_command(command)?;
    let child_id = child.id();
    let reap_probe = child
        .take_reap_probe()
        .ok_or_else(|| FixtureError::new("timed child reap probe was absent"))?;
    let error = match child.wait_with_output(Duration::ZERO) {
        Err(error) => error,
        Ok(_) => return Err(FixtureError::new("timed child unexpectedly exited")),
    };
    let reaped = reap_probe.wait()?;
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert_eq!(reaped.child_id(), child_id);
    assert!(!reaped.status().success());
    Ok(())
}

#[test]
fn backend_finish_success_joins_worker() -> Result<(), FixtureError> {
    let mut backend = Backend::one("complete");
    let join_probe = backend
        .take_join_probe()
        .ok_or_else(|| FixtureError::new("successful backend join probe was absent"))?;
    let response = request_tcp(backend.addr(), "GET", "backend.test", "/complete")?;
    let report = backend.finish()?;
    let shutdown = join_probe.wait()?;
    assert_eq!(response.status, 200);
    assert_eq!(&*response.body, "complete");
    assert!(report.request_paths().eq(["/complete"]));
    assert_eq!(shutdown, BackendShutdown::ListenerReleasedAndWorkerJoined);
    Ok(())
}

#[test]
fn backend_finish_joins_worker_before_propagating_request_failure() -> Result<(), FixtureError> {
    let mut backend = Backend::one("unused");
    let join_probe = backend
        .take_join_probe()
        .ok_or_else(|| FixtureError::new("backend join probe was absent"))?;
    let stream = std::net::TcpStream::connect(backend.addr())?;
    stream.shutdown(std::net::Shutdown::Both)?;
    let error = match backend.finish() {
        Ok(_) => {
            return Err(FixtureError::new(
                "backend worker failure was not propagated",
            ));
        }
        Err(error) => error,
    };
    assert!(
        matches!(error, BackendError::Request(ref source) if source.kind() == std::io::ErrorKind::UnexpectedEof)
    );
    assert_eq!(
        join_probe.wait()?,
        BackendShutdown::ListenerReleasedAndWorkerJoined
    );
    Ok(())
}

#[test]
fn backend_finish_timeout_stops_and_joins_worker() -> Result<(), FixtureError> {
    let mut backend = Backend::one_with_completion_timeout("unused", Duration::ZERO);
    let join_probe = backend
        .take_join_probe()
        .ok_or_else(|| FixtureError::new("backend join probe was absent"))?;
    let error = match backend.finish() {
        Ok(_) => {
            return Err(FixtureError::new(
                "incomplete backend unexpectedly finished",
            ));
        }
        Err(error) => error,
    };
    assert!(matches!(
        error,
        BackendError::CompletionTimeout {
            expected_requests: 1,
            completed_requests: 0
        }
    ));
    assert_eq!(
        join_probe.wait()?,
        BackendShutdown::ListenerReleasedAndWorkerJoined
    );
    Ok(())
}

#[test]
fn backend_drop_joins_worker_after_assertion_panic() -> Result<(), FixtureError> {
    let mut join_probe = None;
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut backend = Backend::one("unused");
        join_probe = backend.take_join_probe();
        assert_eq!(std::process::id(), 0, "simulated backend assertion failure");
    }));
    assert!(panic_result.is_err());
    assert_eq!(
        join_probe
            .ok_or_else(|| FixtureError::new("backend join probe was not retained"))?
            .wait()?,
        BackendShutdown::ListenerReleasedAndWorkerJoined
    );
    Ok(())
}

#[test]
fn readiness_timeout_kills_and_reaps_child() -> Result<(), FixtureError> {
    let root = tempfile::tempdir()?;
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 30"]);
    let readiness = ReadinessTarget::Unix(root.path().join("never-ready.sock"));
    let mut child = ChildGuard::spawn_command_with_readiness(command, readiness)?;
    let child_id = child.id();
    let reap_probe = child
        .take_reap_probe()
        .ok_or_else(|| FixtureError::new("readiness reap probe was absent"))?;
    let error = match child.wait_until_ready_for(Duration::ZERO) {
        Ok(()) => {
            return Err(FixtureError::new(
                "never-ready child unexpectedly became ready",
            ));
        }
        Err(error) => error,
    };
    assert!(
        error.contains("before timeout"),
        "unexpected error: {error}"
    );
    let reaped = reap_probe.wait()?;
    assert_eq!(reaped.child_id(), child_id);
    assert!(!reaped.status().success());
    Ok(())
}

#[test]
fn response_reader_times_out_on_incomplete_declared_body() -> Result<(), FixtureError> {
    let (mut reader, mut writer) = std::os::unix::net::UnixStream::pair()?;
    reader.set_read_timeout(Some(Duration::from_millis(50)))?;
    writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc")?;
    let error = match read_response(&mut reader) {
        Ok(_) => {
            return Err(FixtureError::new(
                "incomplete response unexpectedly completed",
            ));
        }
        Err(error) => error,
    };
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ),
        "unexpected incomplete response error: {error}"
    );
    drop(writer);
    Ok(())
}

#[test]
fn backend_worker_owns_bound_listener_until_shutdown() -> Result<(), FixtureError> {
    let backend = Backend::one("unused");
    let bind_error = match std::net::TcpListener::bind(backend.addr()) {
        Ok(_) => return Err(FixtureError::new("backend listener released its address")),
        Err(error) => error,
    };
    assert_eq!(bind_error.kind(), std::io::ErrorKind::AddrInUse);
    backend.stop()?;
    Ok(())
}

struct HeaderCapacityReader {
    remaining: usize,
    first_read: bool,
}

impl HeaderCapacityReader {
    fn new(capacity: usize) -> Self {
        Self {
            remaining: capacity,
            first_read: true,
        }
    }
}

impl Read for HeaderCapacityReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.len() > self.remaining {
            return Err(std::io::Error::other(format!(
                "reader was offered {} bytes with {} bytes remaining",
                buffer.len(),
                self.remaining
            )));
        }
        let count = match self.first_read {
            true => 1,
            false => buffer.len(),
        };
        buffer[..count].fill(b'x');
        self.first_read = false;
        self.remaining -= count;
        Ok(count)
    }
}

#[test]
fn response_header_reads_never_exceed_remaining_capacity() -> Result<(), FixtureError> {
    let mut reader = HeaderCapacityReader::new(64 * 1024);
    let error = match read_response(&mut reader) {
        Ok(_) => return Err(FixtureError::new("oversized headers unexpectedly parsed")),
        Err(error) => error,
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        error.to_string().contains("headers exceeded size limit"),
        "unexpected bounded header error: {error}"
    );
    Ok(())
}
