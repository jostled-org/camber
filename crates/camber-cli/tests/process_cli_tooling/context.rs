use std::process::Command;
use std::time::Duration;

use crate::support::{FixtureError, run_command, run_command_with_timeout};

fn camber_bin() -> &'static str {
    env!("CARGO_BIN_EXE_camber")
}

#[test]
fn context_command_generates_llms_txt() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;

    let mut command = Command::new(camber_bin());
    command.args(["context"]).current_dir(dir.path());
    let output = run_command(command)?;

    assert!(
        output.status.success(),
        "camber context failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let llms_path = dir.path().join("llms.txt");
    assert!(llms_path.exists(), "llms.txt was not created");

    let content = std::fs::read_to_string(&llms_path)?;
    assert!(content.contains("http::serve"), "missing http::serve");
    assert!(content.contains("Router::new"), "missing Router::new");
    assert!(content.contains("router.get("), "missing router.get");
    assert!(content.contains("Response::text"), "missing Response::text");
    assert!(content.contains("Response::json"), "missing Response::json");
    assert!(content.contains("spawn"), "missing spawn");
    assert!(content.contains("channel"), "missing channel");
    assert!(content.contains("Request"), "missing Request");
    assert!(content.contains("RuntimeError"), "missing RuntimeError");
    assert!(
        content.contains("Anti-pattern")
            || content.contains("anti-pattern")
            || content.contains("Avoid"),
        "missing anti-patterns section"
    );
    Ok(())
}

#[test]
fn context_overwrites_existing_file() -> Result<(), FixtureError> {
    let dir = tempfile::tempdir()?;
    let llms_path = dir.path().join("llms.txt");
    std::fs::write(&llms_path, "stale content from previous version")?;

    let mut command = Command::new(camber_bin());
    command.args(["context"]).current_dir(dir.path());
    let output = run_command(command)?;

    assert!(
        output.status.success(),
        "camber context failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let content = std::fs::read_to_string(&llms_path)?;
    assert!(
        !content.contains("stale content"),
        "file was not overwritten"
    );
    assert!(
        content.contains("http::serve"),
        "fresh content missing http::serve"
    );
    Ok(())
}

#[test]
fn command_fixture_reports_success_and_bounds_captured_output() -> Result<(), FixtureError> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "count=0; while [ $count -lt 70000 ]; do printf x; count=$((count + 1)); done; printf err >&2",
    ]);

    let output = run_command_with_timeout(command, Duration::from_secs(2))?;

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 64 * 1024);
    assert!(output.stdout_truncated);
    assert_eq!(&*output.stderr, b"err");
    assert!(!output.stderr_truncated);
    Ok(())
}
