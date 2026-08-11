use std::fs;
use std::io::{self, Read};
#[cfg(unix)]
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const FIXTURE_TIMEOUT: Duration = Duration::from_secs(300);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PROCESS_REAP_TIMEOUT: Duration = Duration::from_secs(5);
static TARGET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TemporaryTarget {
    path: PathBuf,
}

struct GeneratedLock {
    path: PathBuf,
    existed: bool,
}

impl GeneratedLock {
    fn track(fixture: &Path) -> Self {
        let path = fixture.join("Cargo.lock");
        let existed = path.exists();
        Self { path, existed }
    }
}

impl Drop for GeneratedLock {
    fn drop(&mut self) {
        match self.existed {
            true => {}
            false => remove_generated_lock(&self.path),
        }
    }
}

fn remove_generated_lock(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => std::process::abort(),
    }
}

impl TemporaryTarget {
    fn create() -> io::Result<Self> {
        let base = std::env::temp_dir();
        let process = std::process::id();

        for _ in 0..128 {
            let sequence = TARGET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("camber-macros-{process}-{sequence}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve an isolated Cargo target directory",
        ))
    }
}

impl Drop for TemporaryTarget {
    fn drop(&mut self) {
        if fs::remove_dir_all(&self.path).is_err() {
            std::process::abort();
        }
    }
}

struct CargoOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
    timed_out: bool,
}

struct FixtureProcess {
    child: Child,
    reaped: bool,
}

impl FixtureProcess {
    fn spawn(fixture: &Path, target: &Path, cargo_arguments: &[&str]) -> io::Result<Self> {
        let mut command = Command::new("cargo");
        command
            .args(cargo_arguments)
            .arg("--offline")
            .arg("--message-format=short")
            .current_dir(fixture)
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_TERM_COLOR", "never")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        Self::start(command)
    }

    fn start(mut command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        configure_process_tree(&mut command);

        Ok(Self {
            child: command.spawn()?,
            reaped: false,
        })
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped = status.is_some();
        Ok(status)
    }

    fn terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
        let termination = terminate_process_tree(&mut self.child);
        let status = self.wait_bounded(PROCESS_REAP_TIMEOUT);

        match (termination, status) {
            (_, Ok(status)) => Ok(status),
            (Ok(()), Err(error)) | (Err(error), Err(_)) => Err(error),
        }
    }

    fn wait_bounded(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::other("fixture process reap deadline overflow"))?;
        loop {
            match self.try_wait()? {
                Some(status) => return Ok(status),
                None if Instant::now() < deadline => thread::sleep(PROCESS_POLL_INTERVAL),
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("fixture process {} was not reaped", self.child.id()),
                    ));
                }
            }
        }
    }

    fn terminate_unreaped(&mut self) {
        let _ = terminate_process_tree(&mut self.child);
        match self.wait_bounded(PROCESS_REAP_TIMEOUT) {
            Ok(_) => {}
            Err(_) => std::process::abort(),
        }
    }
}

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        match self.reaped {
            true => {}
            false => self.terminate_unreaped(),
        }
    }
}

#[cfg(unix)]
fn configure_process_tree(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(unix)]
fn terminate_process_tree(child: &mut Child) -> io::Result<()> {
    let process_group = format!("-{}", child.id());
    // `--` stops option parsing before the leading-dash process-group operand.
    // Without it, util-linux `kill` delivers the signal but exits non-zero, so
    // the success branch never runs on Linux and the fallback kills only the
    // immediate child, leaking any descendants in the group.
    let status = Command::new("kill")
        .args(["-KILL", "--", process_group.as_str()])
        .status();

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) | Err(_) => child.kill(),
    }
}

#[cfg(windows)]
fn terminate_process_tree(child: &mut Child) -> io::Result<()> {
    let process_id = child.id().to_string();
    let status = Command::new("taskkill")
        .args(["/F", "/T", "/PID", process_id.as_str()])
        .status();

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) | Err(_) => child.kill(),
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(child: &mut Child) -> io::Result<()> {
    child.kill()
}

#[cfg(unix)]
fn descendant_pipe_failure(process_id: u32, read_error: io::Error) -> io::Error {
    let process_id = process_id.to_string();
    let cleanup = Command::new("kill")
        .args(["-KILL", process_id.as_str()])
        .status();

    match cleanup {
        Ok(status) if status.success() => read_error,
        Ok(status) => io::Error::other(format!(
            "descendant pipe remained open: {read_error}; cleanup exited {status}"
        )),
        Err(cleanup_error) => io::Error::other(format!(
            "descendant pipe remained open: {read_error}; cleanup failed: {cleanup_error}"
        )),
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read_pipe<R>(mut pipe: R) -> io::Result<Vec<u8>>
where
    R: Read,
{
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn run_fixture(name: &str, cargo_arguments: &[&str]) -> io::Result<CargoOutput> {
    let target = TemporaryTarget::create()?;
    let fixture = fixture_path(name);
    let generated_lock = GeneratedLock::track(&fixture);
    let started = Instant::now();
    let mut child = FixtureProcess::spawn(&fixture, &target.path, cargo_arguments)?;

    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Cargo stdout pipe was not captured"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Cargo stderr pipe was not captured"))?;
    let stdout_reader = thread::spawn(move || read_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_pipe(stderr));

    let (status, timed_out) = loop {
        match child.try_wait()? {
            Some(status) => break (status, false),
            None if started.elapsed() >= FIXTURE_TIMEOUT => {
                break (child.terminate_and_wait()?, true);
            }
            None => thread::sleep(PROCESS_POLL_INTERVAL),
        }
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("Cargo stdout reader panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("Cargo stderr reader panicked"))??;
    drop(generated_lock);

    Ok(CargoOutput {
        status,
        stdout,
        stderr,
        elapsed: started.elapsed(),
        timed_out,
    })
}

fn assert_completed(output: &CargoOutput) {
    assert!(
        !output.timed_out,
        "fixture timed out\n{}",
        command_report(output)
    );
}

fn assert_passed(output: &CargoOutput) {
    assert_completed(output);
    assert!(
        output.status.success(),
        "fixture failed\n{}",
        command_report(output)
    );
}

fn assert_failed(output: &CargoOutput) {
    assert_completed(output);
    assert!(
        !output.status.success(),
        "fixture compiled\n{}",
        command_report(output)
    );
}

fn assert_diagnostic(
    output: &CargoOutput,
    source_line: usize,
    source_column: usize,
    fragments: &[&str],
) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let location = format!("src/case.rs:{source_line}:{source_column}:");
    let matched = stderr
        .lines()
        .any(|line| line.contains(&location) && fragments.iter().all(|part| line.contains(part)));

    assert!(
        matched,
        "missing diagnostic at {location} containing {fragments:?}\n{}",
        command_report(output)
    );
}

fn assert_stdout(output: &CargoOutput, fragments: &[&str]) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    fragments.iter().for_each(|fragment| {
        assert!(
            stdout.contains(fragment),
            "missing stdout fragment {fragment:?}\n{}",
            command_report(output)
        );
    });
}

fn command_report(output: &CargoOutput) -> String {
    format!(
        "status: {}\ntimed out: {}\nelapsed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        output.timed_out,
        output.elapsed,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn temporary_target_drop_removes_tree_after_unwind() -> io::Result<()> {
    let fixture_workspace = TemporaryTarget::create()?;
    let fixture = fixture_workspace.path.join("fixture");
    fs::create_dir(&fixture)?;

    let generated_lock = GeneratedLock::track(&fixture);
    let generated_lock_path = fixture.join("Cargo.lock");
    fs::write(&generated_lock_path, b"generated fixture lock")?;

    let target = TemporaryTarget::create()?;
    let target_path = target.path.clone();
    let target_artifacts = target_path.join("debug/deps");
    fs::create_dir_all(&target_artifacts)?;
    fs::write(target_artifacts.join("fixture-artifact"), b"artifact")?;

    let assertion = std::panic::catch_unwind(move || {
        assert!(
            target.path.as_os_str().is_empty(),
            "exercise cleanup while unwinding an assertion"
        );
        drop(generated_lock);
    });

    assert!(assertion.is_err(), "cleanup witness did not unwind");
    assert!(!target_path.exists(), "temporary target tree survived");
    assert!(
        !generated_lock_path.exists(),
        "generated fixture lock survived"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn timeout_cleanup_terminates_descendant_processes() -> io::Result<()> {
    let (stdout, child_stdout) = UnixStream::pair()?;
    stdout.set_read_timeout(Some(PROCESS_REAP_TIMEOUT))?;
    let mut command = Command::new("sh");
    command
        .args(["-c", "sleep 120 & echo $!; wait"])
        .stdout(Stdio::from(OwnedFd::from(child_stdout)))
        .stderr(Stdio::null());
    let mut process = FixtureProcess::start(command)?;
    let mut stdout = BufReader::new(stdout);
    let mut grandchild_line = String::new();
    stdout.read_line(&mut grandchild_line)?;
    let grandchild = grandchild_line
        .trim()
        .parse::<u32>()
        .map_err(io::Error::other)?;

    process.terminate_and_wait()?;

    let mut trailing_output = Vec::new();
    match stdout.read_to_end(&mut trailing_output) {
        Ok(_) => assert!(
            trailing_output.is_empty(),
            "descendant process {grandchild} emitted unexpected output"
        ),
        Err(error) => return Err(descendant_pipe_failure(grandchild, error)),
    }
    Ok(())
}

#[test]
fn unsupported_attribute_arguments_are_rejected() -> io::Result<()> {
    let output = run_fixture("unsupported_arguments", &["check"])?;
    assert_failed(&output);
    assert_diagnostic(&output, 6, 24, &["camber::test", "attribute arguments"]);
    Ok(())
}

#[test]
fn renamed_dependencies_are_hygienic_and_execute() -> io::Result<()> {
    let output = run_fixture("renamed_runtime", &["test"])?;
    assert_passed(&output);
    assert_stdout(
        &output,
        &[
            "renamed_runtime_dependency_executes_generated_test ... ok",
            "renamed_proc_macro_dependency_executes_generated_test ... ok",
            "lexical_names_do_not_capture_generated_runtime_path ... ok",
            "preserves_should_panic_attribute - should panic ... ok",
            "preserves_ignore_attribute ... ignored",
        ],
    );
    Ok(())
}

#[test]
fn missing_runtime_dependency_reports_lookup_failure() -> io::Result<()> {
    let output = run_fixture("missing_runtime", &["check"])?;
    assert_failed(&output);
    assert_diagnostic(
        &output,
        1,
        1,
        &["camber::test", "could not resolve", "runtime dependency"],
    );
    Ok(())
}

#[test]
fn non_async_function_reports_the_function_span() -> io::Result<()> {
    let output = run_fixture("non_async", &["check"])?;
    assert_failed(&output);
    assert_diagnostic(&output, 7, 1, &["camber::test requires an async fn"]);
    Ok(())
}

#[test]
fn invalid_signatures_report_each_invalid_part() -> io::Result<()> {
    let output = run_fixture("invalid_signatures", &["check"])?;
    assert_failed(&output);
    assert_diagnostic(&output, 7, 25, &["camber::test", "parameters"]);
    assert_diagnostic(&output, 10, 33, &["camber::test", "generic parameters"]);
    assert_diagnostic(&output, 13, 29, &["camber::test", "return type"]);
    assert_diagnostic(&output, 18, 7, &["camber::test", "unsafe"]);
    assert_diagnostic(&output, 21, 1, &["camber::test", "const"]);
    assert_diagnostic(&output, 24, 7, &["camber::test", "ABI"]);
    assert_diagnostic(&output, 28, 1, &["camber::test", "where clause"]);
    Ok(())
}
