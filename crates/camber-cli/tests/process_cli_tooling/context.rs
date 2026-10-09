use std::process::Command;
use std::time::Duration;

use crate::support::generated_project::{camber_context, in_temp_dir};
use crate::support::{FixtureError, run_command_with_timeout};

/// How long the capture fixture's own command has to run to completion.
///
/// Its own number rather than `CHILD_EXIT_TIMEOUT`. That constant bounds a
/// reap — a child already told to exit, which a healthy host completes in
/// milliseconds — and this bounds a whole `/bin/sh` run that builds and prints
/// 128 KiB. One number carrying both claims is one number that cannot be tuned
/// for either: shortening the reap bound would start failing this command on a
/// loaded runner, and lengthening it for this command would let a stuck child
/// hang the suite for longer than the reap ever needs.
const CAPTURE_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn context_command_generates_llms_txt() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        camber_context(dir)?;

        let llms_path = dir.join("llms.txt");
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
    })
}

#[test]
fn context_overwrites_existing_file() -> Result<(), FixtureError> {
    in_temp_dir(|dir| {
        let llms_path = dir.join("llms.txt");
        std::fs::write(&llms_path, "stale content from previous version")?;

        camber_context(dir)?;

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
    })
}

#[test]
fn command_fixture_reports_success_and_bounds_captured_output() -> Result<(), FixtureError> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "chunk=x; doublings=0; while [ $doublings -lt 16 ]; do chunk=\"$chunk$chunk\"; doublings=$((doublings + 1)); done; printf %s%s \"$chunk\" \"$chunk\"; printf err >&2",
    ]);

    let output = run_command_with_timeout(command, CAPTURE_COMMAND_TIMEOUT)?;

    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 64 * 1024);
    assert!(output.stdout_truncated);
    assert_eq!(&*output.stderr, b"err");
    assert!(!output.stderr_truncated);
    Ok(())
}
