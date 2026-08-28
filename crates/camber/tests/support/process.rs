use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CHILD_MODE_ENV: &str = "CAMBER_FIXTURE_PRIVATE_CHILD_MODE";
const CHILD_NONCE_ENV: &str = "CAMBER_FIXTURE_PRIVATE_CHILD_NONCE";
const CHILD_PARENT_ID_ENV: &str = "CAMBER_FIXTURE_PRIVATE_PARENT_ID";
const OUTPUT_CAPTURE_LIMIT: usize = 64 * 1024;
static CHILD_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("child process I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("child did not report readiness before {timeout:?}")]
    ReadinessTimeout { timeout: Duration },
    #[error("child stdout closed before readiness")]
    ReadinessClosed,
    #[error("child did not exit before {timeout:?}")]
    ExitTimeout { timeout: Duration },
    #[error("child output reader did not join before {timeout:?}")]
    ReaderTimeout { timeout: Duration },
    #[error("child output reader panicked")]
    ReaderPanicked,
    #[error("child output exceeded the {limit}-byte capture limit")]
    OutputLimit { limit: usize },
    #[error("child reap probe did not complete before {timeout:?}")]
    ReapProbeTimeout { timeout: Duration },
    #[error("child cleanup failed: {message}")]
    Cleanup { message: Box<str> },
}

#[derive(Clone, Copy, Debug)]
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

pub struct ReapProbe {
    receiver: Receiver<Result<ReapedChild, Box<str>>>,
}

pub struct IsolatedRun {
    status: ExitStatus,
    stdout: Box<[u8]>,
    stderr: Box<[u8]>,
}

impl IsolatedRun {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

impl ReapProbe {
    pub fn wait(self, timeout: Duration) -> Result<ReapedChild, ProcessError> {
        match self.receiver.recv_timeout(timeout) {
            Ok(Ok(reaped)) => Ok(reaped),
            Ok(Err(message)) => Err(ProcessError::Cleanup { message }),
            Err(_) => Err(ProcessError::ReapProbeTimeout { timeout }),
        }
    }
}

pub struct ChildGuard {
    child: Option<Child>,
    lines: Receiver<Box<str>>,
    stdout_reader: Option<JoinHandle<Result<Box<[u8]>, ProcessError>>>,
    stderr_reader: Option<JoinHandle<Result<Box<[u8]>, ProcessError>>>,
    stdout: Box<[u8]>,
    stderr: Box<[u8]>,
    reap_sender: Option<Sender<Result<ReapedChild, Box<str>>>>,
    reap_receiver: Option<Receiver<Result<ReapedChild, Box<str>>>>,
    cleanup_timeout: Duration,
}

impl ChildGuard {
    pub fn spawn_exact_current(
        test_name: &str,
        mode: &str,
        cleanup_timeout: Duration,
    ) -> Result<Self, ProcessError> {
        let nonce = CHILD_NONCE.fetch_add(1, Ordering::Relaxed);
        let nonce = format!("{}-{nonce}", std::process::id());
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--exact")
            .arg(test_name)
            .arg("--nocapture")
            .env_remove(CHILD_MODE_ENV)
            .env_remove(CHILD_NONCE_ENV)
            .env_remove(CHILD_PARENT_ID_ENV)
            .env(CHILD_MODE_ENV, mode)
            .env(CHILD_NONCE_ENV, nonce)
            .env(CHILD_PARENT_ID_ENV, std::process::id().to_string())
            // Only the stdin `spawn` does not set: it pipes both output streams
            // itself, because it captures them.
            .stdin(Stdio::null());
        Self::spawn(command, cleanup_timeout)
    }

    pub fn spawn(mut command: Command, cleanup_timeout: Duration) -> Result<Self, ProcessError> {
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("child stdout was not piped"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("child stderr was not piped"))?;
        let (line_sender, line_receiver) = mpsc::channel();
        let stdout_reader = std::thread::spawn(move || capture_output(stdout, Some(line_sender)));
        let stderr_reader = std::thread::spawn(move || capture_output(stderr, None));
        let (reap_sender, reap_receiver) = mpsc::channel();
        Ok(Self {
            child: Some(child),
            lines: line_receiver,
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
            stdout: Box::new([]),
            stderr: Box::new([]),
            reap_sender: Some(reap_sender),
            reap_receiver: Some(reap_receiver),
            cleanup_timeout,
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

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Wait for the line carrying `expected`, and hand it back.
    ///
    /// The form for a marker that carries a payload — an address the child
    /// bound, say — so a caller that needs what the line said reads it here
    /// instead of racing a second read against the same stream.
    pub fn await_line(&self, expected: &str, timeout: Duration) -> Result<Box<str>, ProcessError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.lines.recv_timeout(super::http::remaining(deadline)) {
                Ok(line) if line.contains(expected) => return Ok(line.into()),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(ProcessError::ReadinessTimeout { timeout });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProcessError::ReadinessClosed);
                }
            }
        }
    }

    /// Wait for the line carrying `expected`, for a caller the marker alone
    /// answers.
    pub fn wait_for_line(&self, expected: &str, timeout: Duration) -> Result<(), ProcessError> {
        self.await_line(expected, timeout).map(drop)
    }

    pub fn wait_for_readiness(
        &mut self,
        expected: &str,
        timeout: Duration,
    ) -> Result<(), ProcessError> {
        match self.wait_for_line(expected, timeout) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.shutdown_reporting_failure();
                Err(error)
            }
        }
    }

    pub fn wait_bounded(&mut self, timeout: Duration) -> Result<ExitStatus, ProcessError> {
        match self.wait_for_exit(timeout)? {
            Some(status) => {
                self.finish_and_report(status)?;
                Ok(status)
            }
            None => {
                self.shutdown_reporting_failure();
                Err(ProcessError::ExitTimeout { timeout })
            }
        }
    }

    /// Shut the child down on behalf of a caller that already holds the better
    /// diagnosis, reporting any cleanup fault through the reap probe.
    ///
    /// A readiness wait that expired and an exit bound that was not met each
    /// name what the test was actually waiting for. Returning the cleanup fault
    /// in place of that would answer a question nobody asked — and one test
    /// asserts on the `ExitTimeout` exactly. The reap probe is where a cleanup
    /// fault is read from, so it goes there instead of over the top.
    fn shutdown_reporting_failure(&mut self) {
        if let Err(error) = self.shutdown() {
            self.report_cleanup_failure(&error);
        }
    }

    pub fn shutdown(&mut self) -> Result<(), ProcessError> {
        let status = match self.try_wait()? {
            Some(status) => status,
            None => self.kill_and_wait()?,
        };
        self.finish_and_report(status)
    }

    fn kill_and_wait(&mut self) -> Result<ExitStatus, ProcessError> {
        let kill_result = self.child.as_mut().map(Child::kill);
        match kill_result {
            Some(Err(error)) => self.status_after_kill_error(error),
            Some(Ok(())) | None => {
                self.wait_for_exit(self.cleanup_timeout)?
                    .ok_or(ProcessError::ExitTimeout {
                        timeout: self.cleanup_timeout,
                    })
            }
        }
    }

    fn status_after_kill_error(&mut self, error: io::Error) -> Result<ExitStatus, ProcessError> {
        match self.try_wait()? {
            Some(status) => Ok(status),
            None => Err(ProcessError::Io(error)),
        }
    }

    fn finish_and_report(&mut self, status: ExitStatus) -> Result<(), ProcessError> {
        let result = self.finish_reap(status);
        if let Err(error) = &result {
            self.report_cleanup_failure(error);
        }
        result
    }

    fn try_wait(&mut self) -> Result<Option<ExitStatus>, ProcessError> {
        match self.child.as_mut() {
            Some(child) => child.try_wait().map_err(ProcessError::Io),
            None => Ok(None),
        }
    }

    /// Wait for the child to exit, bounded.
    ///
    /// The shared bounded poll clamps its last sleep to what is left of
    /// `timeout`, so an exit that lands just inside the caller's budget is still
    /// seen. Sleeping a whole fixed interval here used to overshoot it.
    fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, ProcessError> {
        super::http::poll_value(timeout, || self.try_wait().transpose()).transpose()
    }

    fn finish_reap(&mut self, status: ExitStatus) -> Result<(), ProcessError> {
        let child_id = self.id();
        // Given up only once the readers are in. `Drop` keys its second attempt
        // off the child still being held, so releasing it before a reader join
        // that then times out would leave both reader threads running with
        // nothing left to join them for the rest of the binary.
        self.join_readers()?;
        self.child.take();
        if let Some(sender) = self.reap_sender.take() {
            let _ = sender.send(Ok(ReapedChild { child_id, status }));
        }
        Ok(())
    }

    fn join_readers(&mut self) -> Result<(), ProcessError> {
        let timeout = self.cleanup_timeout;
        let finished = super::http::poll_until(timeout, || {
            self.stdout_reader
                .as_ref()
                .is_none_or(JoinHandle::is_finished)
                && self
                    .stderr_reader
                    .as_ref()
                    .is_none_or(JoinHandle::is_finished)
        });
        match finished {
            true => {}
            false => return Err(ProcessError::ReaderTimeout { timeout }),
        }
        self.stdout = join_output_reader(&mut self.stdout_reader)?;
        self.stderr = join_output_reader(&mut self.stderr_reader)?;
        Ok(())
    }

    fn report_cleanup_failure(&mut self, error: &ProcessError) {
        if let Some(sender) = self.reap_sender.take() {
            let _ = sender.send(Err(error.to_string().into_boxed_str()));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.is_none() {
            return;
        }
        if let Err(error) = self.shutdown() {
            self.report_cleanup_failure(&error);
        }
    }
}

fn capture_output<R: Read>(
    mut reader: R,
    line_sender: Option<Sender<Box<str>>>,
) -> Result<Box<[u8]>, ProcessError> {
    let mut captured = Vec::new();
    let mut line = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            send_final_line(line_sender.as_ref(), &line);
            return Ok(captured.into_boxed_slice());
        }
        if captured
            .len()
            .checked_add(count)
            .is_none_or(|length| length > OUTPUT_CAPTURE_LIMIT)
        {
            return Err(ProcessError::OutputLimit {
                limit: OUTPUT_CAPTURE_LIMIT,
            });
        }
        captured.extend_from_slice(&chunk[..count]);
        if let Some(sender) = line_sender.as_ref() {
            capture_lines(sender, &chunk[..count], &mut line);
        }
    }
}

fn send_final_line(sender: Option<&Sender<Box<str>>>, line: &[u8]) {
    match line.is_empty() {
        true => {}
        false => send_line(sender, line),
    }
}

fn capture_lines(sender: &Sender<Box<str>>, bytes: &[u8], line: &mut Vec<u8>) {
    bytes.iter().for_each(|byte| match *byte {
        b'\n' => {
            send_line(Some(sender), line);
            line.clear();
        }
        byte => line.push(byte),
    });
}

fn send_line(sender: Option<&Sender<Box<str>>>, line: &[u8]) {
    if let Some(sender) = sender {
        let line = String::from_utf8_lossy(line)
            .trim_end_matches('\r')
            .to_owned()
            .into_boxed_str();
        let _ = sender.send(line);
    }
}

fn join_output_reader(
    reader: &mut Option<JoinHandle<Result<Box<[u8]>, ProcessError>>>,
) -> Result<Box<[u8]>, ProcessError> {
    match reader.take() {
        Some(reader) => reader.join().map_err(|_| ProcessError::ReaderPanicked)?,
        None => Ok(Box::new([])),
    }
}

pub fn is_private_child(expected_mode: &str) -> bool {
    let mode = std::env::var(CHILD_MODE_ENV).ok();
    let nonce = std::env::var(CHILD_NONCE_ENV).ok();
    matches!((mode.as_deref(), nonce.as_deref()), (Some(mode), Some(nonce)) if mode == expected_mode && !nonce.is_empty())
}

/// Wait out a spawned child, leaving the guard with the caller.
///
/// Split from `run_isolated_exact` so a caller that wants the child's own words
/// on the failure path can still reach them: both failure modes here run the
/// guard's shutdown, which is what fills its captured streams, and a caller
/// handed only the error has already lost them.
fn drive_isolated(
    child: &mut ChildGuard,
    assertion_marker: &str,
    timeout: Duration,
) -> Result<ExitStatus, ProcessError> {
    child.wait_for_readiness(assertion_marker, timeout)?;
    child.wait_bounded(timeout)
}

pub fn run_isolated_exact(
    test_name: &str,
    mode: &str,
    assertion_marker: &str,
    timeout: Duration,
) -> Result<IsolatedRun, ProcessError> {
    let mut child = ChildGuard::spawn_exact_current(test_name, mode, timeout)?;
    let status = drive_isolated(&mut child, assertion_marker, timeout)?;
    Ok(IsolatedRun {
        status,
        stdout: std::mem::take(&mut child.stdout),
        stderr: std::mem::take(&mut child.stderr),
    })
}

/// Run `body` in a private child process, or drive that child from the parent.
///
/// One function, two sides. In the child it runs `body`, prints `marker` so the
/// parent's readiness wait completes, and reports `true` so the caller returns
/// without running the parent's assertions. In the parent it runs the child
/// bounded by `bound`, fails with the child's stderr when the child failed, and
/// reports `false` so the caller goes on to assert whatever it owns.
///
/// A process-isolated case is otherwise fifteen lines of preamble that every
/// such test restates, and a copy that forgets the flush hangs its parent on a
/// marker still sitting in the child's buffer.
///
/// The child's stderr is quoted on **both** failure paths, not only the one that
/// reached an exit status. A body that fails its assertions never prints the
/// marker, so the parent's readiness wait is what ends — and reporting only that
/// wait renders a failed claim as a process that would not start, which is the
/// one thing a falsification receipt taken through here must not be confused
/// with.
pub fn run_in_child(
    test_name: &str,
    mode: &str,
    marker: &str,
    bound: Duration,
    body: impl FnOnce(),
) -> bool {
    if is_private_child(mode) {
        body();
        println!("{marker}");
        // Flushed here, not left to exit: the parent waits on this line, and a
        // marker still in the child's buffer reads as a child that never got
        // there.
        io::Write::flush(&mut std::io::stdout()).expect("the child could not flush its marker");
        return true;
    }
    assert_child_succeeded(test_name, mode, marker, bound);
    false
}

/// Run `body` in a private child that is meant to outlive its own assertions.
///
/// The containment counterpart of [`run_in_child`], for a subject the child
/// cannot take back: a blocking application callback Camber has deliberately
/// stopped waiting for. Such a child cannot exit on its own — exiting would be
/// indistinguishable from the callback having cooperated — so it says its
/// assertions passed and then parks.
///
/// In the child it runs `body`, prints `marker`, flushes it, and parks forever.
/// In the parent it waits `bound` for that marker, quotes the child's stderr if
/// it never came, and otherwise ends the child through the public
/// [`ChildGuard::shutdown`] and requires the reap to complete inside `bound`.
///
/// The kill is cleanup and never evidence. It happens only after the marker has
/// arrived, so nothing the parent asserts depends on how the child ended; a
/// child that had exited by itself would have closed its stdout, and the marker
/// wait would report that instead.
pub fn contain_in_child(
    test_name: &str,
    mode: &str,
    marker: &str,
    bound: Duration,
    body: impl FnOnce(),
) {
    if is_private_child(mode) {
        body();
        println!("{marker}");
        io::Write::flush(&mut std::io::stdout()).expect("the child could not flush its marker");
        park_until_reaped();
    }
    assert_child_contained(test_name, mode, marker, bound);
}

/// Hold this process open for the parent that is about to reap it.
///
/// Parking rather than sleeping a fixed span: the parent owns when this ends,
/// and a span that expired first would let the child exit on its own and turn a
/// containment protocol into a race.
fn park_until_reaped() -> ! {
    loop {
        std::thread::park();
    }
}

/// Drive the contained child from the parent side, then reap it boundedly.
fn assert_child_contained(test_name: &str, mode: &str, marker: &str, bound: Duration) {
    let mut child = ChildGuard::spawn_exact_current(test_name, mode, bound)
        .expect("the contained child could not be started");
    let child_id = child.id();
    let probe = child
        .take_reap_probe()
        .expect("a freshly spawned guard owns its reap probe");
    if let Err(error) = child.wait_for_readiness(marker, bound) {
        panic!(
            "the contained child never reported: {error}\n{}",
            String::from_utf8_lossy(child.stderr())
        );
    }
    child
        .shutdown()
        .expect("the contained child could not be reaped");
    let reaped = probe
        .wait(bound)
        .expect("the contained child's reap did not complete");
    assert_eq!(
        reaped.child_id(),
        child_id,
        "the reap probe reported a different child"
    );
}

/// Run the private child from the parent side, failing with what it said.
fn assert_child_succeeded(test_name: &str, mode: &str, marker: &str, bound: Duration) {
    let mut child = ChildGuard::spawn_exact_current(test_name, mode, bound)
        .expect("the isolated child could not be started");
    let failure = match drive_isolated(&mut child, marker, bound) {
        Ok(status) if status.success() => return,
        Ok(status) => format!("the isolated child exited with {status}"),
        Err(error) => format!("the isolated child never reported: {error}"),
    };
    panic!("{failure}\n{}", String::from_utf8_lossy(child.stderr()));
}

pub fn private_child_parent_id() -> Option<u32> {
    std::env::var(CHILD_PARENT_ID_ENV).ok()?.parse().ok()
}
