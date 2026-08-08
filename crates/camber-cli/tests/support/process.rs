use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(100);
/// How long a child already told to exit has to be reaped.
///
/// Bounds a hang, not a performance expectation: a healthy child is reaped in
/// milliseconds, so this only has to outlast scheduling delay when the full
/// `[ci]` matrix runs every test binary in parallel. It bounds reaps and
/// nothing else — a caller waiting on a command that is still doing its work
/// names its own bound, because that wait is a different claim and the two
/// cannot be tuned through one number.
pub const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const OUTPUT_CAPTURE_LIMIT: usize = 64 * 1024;
const READINESS_RESPONSE_LIMIT: u64 = 64 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

type CapturedOutput = (Box<[u8]>, Box<[u8]>);

pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: Box<[u8]>,
    pub stderr: Box<[u8]>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub fn run_command_with_timeout(command: Command, timeout: Duration) -> io::Result<CommandOutput> {
    let mut child = ChildGuard::spawn_command(command)?;
    child.wait_with_output(timeout)
}

pub fn run_command(command: Command) -> io::Result<CommandOutput> {
    run_command_with_timeout(command, COMMAND_TIMEOUT)
}

pub struct ChildGuard {
    child: Option<Child>,
    readiness: Option<ReadinessTarget>,
    output: OutputCapture,
    reap_sender: Option<Sender<ReapedChild>>,
    reap_receiver: Option<Receiver<ReapedChild>>,
}

#[derive(Clone)]
pub enum ReadinessTarget {
    Unix(PathBuf),
}

pub struct ReapProbe {
    receiver: Receiver<ReapedChild>,
}

impl ReapProbe {
    pub fn wait(self) -> Result<ReapedChild, Box<str>> {
        self.receiver
            .recv_timeout(CHILD_EXIT_TIMEOUT)
            .map_err(|error| {
                format!("wait for serve child reap completion: {error}").into_boxed_str()
            })
    }
}

#[derive(Clone, Copy)]
pub struct ReapedChild {
    child_id: u32,
    status: ExitStatus,
}

impl ReapedChild {
    pub fn child_id(&self) -> u32 {
        self.child_id
    }

    pub fn status(&self) -> ExitStatus {
        self.status
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TerminationKind {
    AlreadyExited,
    Killed,
    NaturalExitAfterObservation,
}

struct OutputCapture {
    stdout: File,
    stderr: File,
}

impl OutputCapture {
    fn attach(command: &mut Command) -> io::Result<Self> {
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        command.stdout(Stdio::from(stdout.try_clone()?));
        command.stderr(Stdio::from(stderr.try_clone()?));
        Ok(Self { stdout, stderr })
    }

    fn read(&mut self) -> io::Result<CapturedOutput> {
        let stdout = read_capture(&mut self.stdout)?;
        let stderr = read_capture(&mut self.stderr)?;
        Ok((stdout, stderr))
    }

    fn read_bounded(&mut self) -> io::Result<(CapturedOutput, bool, bool)> {
        let (stdout, stdout_truncated) = read_bounded_capture(&mut self.stdout)?;
        let (stderr, stderr_truncated) = read_bounded_capture(&mut self.stderr)?;
        Ok(((stdout, stderr), stdout_truncated, stderr_truncated))
    }
}

impl ChildGuard {
    pub fn spawn(
        binary: &Path,
        config_path: &Path,
        readiness: ReadinessTarget,
    ) -> io::Result<Self> {
        let mut command = Command::new(binary);
        command.args(["serve".as_ref(), config_path.as_os_str()]);
        Self::spawn_inner(command, Some(readiness))
    }

    pub fn spawn_command(command: Command) -> io::Result<Self> {
        Self::spawn_inner(command, None)
    }

    pub fn spawn_command_with_readiness(
        command: Command,
        readiness: ReadinessTarget,
    ) -> io::Result<Self> {
        Self::spawn_inner(command, Some(readiness))
    }

    fn spawn_inner(mut command: Command, readiness: Option<ReadinessTarget>) -> io::Result<Self> {
        let output = OutputCapture::attach(&mut command)?;
        let child = command.spawn()?;
        let (reap_sender, reap_receiver) = mpsc::channel();
        Ok(Self {
            child: Some(child),
            readiness,
            output,
            reap_sender: Some(reap_sender),
            reap_receiver: Some(reap_receiver),
        })
    }

    pub fn id(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    pub fn take_reap_probe(&mut self) -> Option<ReapProbe> {
        self.reap_receiver
            .take()
            .map(|receiver| ReapProbe { receiver })
    }

    pub fn wait_until_ready(&mut self) -> Result<(), Box<str>> {
        self.wait_until_ready_for(STARTUP_TIMEOUT)
    }

    pub fn wait_until_ready_for(&mut self, timeout: Duration) -> Result<(), Box<str>> {
        let readiness = self
            .readiness
            .clone()
            .ok_or("serve readiness target is absent")?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.probe_startup(&readiness, deadline) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(start_error) => {
                    return Err(self.cleanup_startup_failure(start_error));
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn probe_startup(
        &mut self,
        readiness: &ReadinessTarget,
        deadline: Instant,
    ) -> Result<bool, Box<str>> {
        let connect_error = match probe_ready(readiness) {
            Ok(()) => return Ok(true),
            Err(error) => error,
        };
        self.check_startup_state(connect_error, deadline)
            .map(|()| false)
    }

    fn cleanup_startup_failure(&mut self, start_error: Box<str>) -> Box<str> {
        match self.shutdown() {
            Ok(()) => start_error,
            Err(cleanup_error) => {
                format!("{start_error}; failed child cleanup: {cleanup_error}").into_boxed_str()
            }
        }
    }

    fn check_startup_state(
        &mut self,
        connect_error: io::Error,
        deadline: Instant,
    ) -> Result<(), Box<str>> {
        let child = self.child.as_mut().ok_or("serve child is not owned")?;
        match child.try_wait() {
            Ok(Some(status)) => Err(self.exited_message(status).into_boxed_str()),
            Ok(None) if Instant::now() < deadline => Ok(()),
            Ok(None) => Err(format!(
                "serve child {} did not become ready at {} before timeout; last connect error: {connect_error}",
                child.id(),
                self.readiness
                    .as_ref()
                    .map_or_else(|| "<absent>".into(), readiness_name)
            )
            .into_boxed_str()),
            Err(error) => Err(format!("inspect serve child: {error}").into_boxed_str()),
        }
    }

    fn exited_message(&mut self, status: ExitStatus) -> String {
        match self.output.read() {
            Ok((_, stderr)) => {
                let stderr = String::from_utf8_lossy(&stderr);
                format!("serve child exited with {status}: {stderr}")
            }
            Err(error) => {
                format!("serve child exited with {status}; output capture failed: {error}")
            }
        }
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.shutdown_with(terminate_and_reap).map(|_| ())
    }

    pub fn wait_with_output(&mut self, timeout: Duration) -> io::Result<CommandOutput> {
        let deadline = Instant::now() + timeout;
        let status = match self.child.as_mut() {
            Some(child) => wait_for_exit(child, deadline),
            None => return Err(io::Error::other("command child is not owned")),
        };
        let status = match status {
            Ok(status) => status,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                self.shutdown()?;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let child_id = self.id();
        self.child.take();
        let output = self.output.read_bounded();
        self.report_reaped(ReapedChild { child_id, status });
        let ((stdout, stderr), stdout_truncated, stderr_truncated) = output?;
        Ok(CommandOutput {
            status,
            stdout,
            stderr,
            stdout_truncated,
            stderr_truncated,
        })
    }

    pub fn shutdown_after_observation<F>(&mut self, release: F) -> io::Result<TerminationKind>
    where
        F: FnOnce(&mut Child) -> io::Result<()>,
    {
        self.shutdown_with(|child| terminate_after_observation(child, release))
    }

    fn shutdown_with<F>(&mut self, terminate: F) -> io::Result<TerminationKind>
    where
        F: FnOnce(&mut Child) -> io::Result<(ExitStatus, TerminationKind)>,
    {
        let result = match self.child.as_mut() {
            Some(child) => terminate(child).map(|(status, kind)| {
                (
                    ReapedChild {
                        child_id: child.id(),
                        status,
                    },
                    kind,
                )
            }),
            None => return Err(io::Error::other("serve child is not owned")),
        };
        let (reaped, kind) = result?;
        self.child.take();
        let output_result = self.output.read().map(|_| ());
        self.report_reaped(reaped);
        output_result.map(|()| kind)
    }

    fn report_reaped(&mut self, reaped: ReapedChild) {
        match self.reap_sender.take() {
            Some(sender) => drop(sender.send(reaped)),
            None => {}
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_some() && self.cleanup_for_drop().is_err() {
            std::process::abort();
        }
    }
}

impl ChildGuard {
    fn cleanup_for_drop(&mut self) -> io::Result<()> {
        let reaped = match self.child.as_mut() {
            Some(child) => cleanup_owned_child(child)?,
            None => return Ok(()),
        };
        self.child.take();
        self.report_reaped(reaped);
        Ok(())
    }
}

fn cleanup_owned_child(child: &mut Child) -> io::Result<ReapedChild> {
    let child_id = child.id();
    let (status, _) = terminate_and_reap(child).or_else(|_| force_kill_and_reap(child))?;
    Ok(ReapedChild { child_id, status })
}

fn terminate_and_reap(child: &mut Child) -> io::Result<(ExitStatus, TerminationKind)> {
    match child.try_wait()? {
        Some(status) => Ok((status, TerminationKind::AlreadyExited)),
        None => kill_and_reap(child),
    }
}

fn kill_and_reap(child: &mut Child) -> io::Result<(ExitStatus, TerminationKind)> {
    let kill_result = child.kill();
    let status = wait_for_exit(child, Instant::now() + CHILD_EXIT_TIMEOUT)?;
    match kill_result {
        Ok(()) => Ok((status, TerminationKind::Killed)),
        Err(_) => Ok((status, TerminationKind::NaturalExitAfterObservation)),
    }
}

fn force_kill_and_reap(child: &mut Child) -> io::Result<(ExitStatus, TerminationKind)> {
    let kill_result = child.kill();
    let wait_result = wait_for_exit(child, Instant::now() + CHILD_EXIT_TIMEOUT);
    match (wait_result, kill_result) {
        (Ok(status), _) => Ok((status, TerminationKind::Killed)),
        (Err(wait_error), Ok(())) => Err(wait_error),
        (Err(wait_error), Err(kill_error)) => Err(io::Error::new(
            wait_error.kind(),
            format!("kill child failed: {kill_error}; reap child failed: {wait_error}"),
        )),
    }
}

fn terminate_after_observation<F>(
    child: &mut Child,
    release: F,
) -> io::Result<(ExitStatus, TerminationKind)>
where
    F: FnOnce(&mut Child) -> io::Result<()>,
{
    match child.try_wait()? {
        Some(status) => return Ok((status, TerminationKind::AlreadyExited)),
        None => release(child)?,
    }
    let status = wait_for_exit(child, Instant::now() + CHILD_EXIT_TIMEOUT)?;
    let _ = child.kill();
    Ok((status, TerminationKind::NaturalExitAfterObservation))
}

fn wait_for_exit(child: &mut Child, deadline: Instant) -> io::Result<ExitStatus> {
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status),
            None if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("serve child {} was not reaped before deadline", child.id()),
                ));
            }
        }
    }
}

fn read_capture(file: &mut File) -> io::Result<Box<[u8]>> {
    file.seek(SeekFrom::Start(0))?;
    let mut captured = Vec::new();
    file.take(OUTPUT_CAPTURE_LIMIT as u64)
        .read_to_end(&mut captured)?;
    Ok(captured.into_boxed_slice())
}

fn read_bounded_capture(file: &mut File) -> io::Result<(Box<[u8]>, bool)> {
    file.seek(SeekFrom::Start(0))?;
    let mut captured = Vec::new();
    file.take((OUTPUT_CAPTURE_LIMIT + 1) as u64)
        .read_to_end(&mut captured)?;
    let truncated = captured.len() > OUTPUT_CAPTURE_LIMIT;
    captured.truncate(OUTPUT_CAPTURE_LIMIT);
    Ok((captured.into_boxed_slice(), truncated))
}

fn probe_ready(readiness: &ReadinessTarget) -> io::Result<()> {
    let ReadinessTarget::Unix(path) = readiness;
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(CONNECT_ATTEMPT_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECT_ATTEMPT_TIMEOUT))?;
    stream.write_all(
        b"GET / HTTP/1.1\r\nHost: camber-readiness.invalid\r\nConnection: close\r\n\r\n",
    )?;
    let mut response = Vec::new();
    stream
        .take(READINESS_RESPONSE_LIMIT + 1)
        .read_to_end(&mut response)?;
    if response.len() as u64 > READINESS_RESPONSE_LIMIT {
        return Err(io::Error::other(
            "serve readiness response exceeded size limit",
        ));
    }
    match response.starts_with(b"HTTP/1.1 ") {
        true => Ok(()),
        false => Err(io::Error::other(
            "serve readiness response was not HTTP/1.1",
        )),
    }
}

fn readiness_name(readiness: &ReadinessTarget) -> String {
    let ReadinessTarget::Unix(path) = readiness;
    path.display().to_string()
}
