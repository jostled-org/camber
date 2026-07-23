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
const POLL_INTERVAL: Duration = Duration::from_millis(10);
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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

    pub fn wait_for_line(&self, expected: &str, timeout: Duration) -> Result<(), ProcessError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) if line.contains(expected) => return Ok(()),
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

    pub fn wait_for_readiness(
        &mut self,
        expected: &str,
        timeout: Duration,
    ) -> Result<(), ProcessError> {
        match self.wait_for_line(expected, timeout) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.shutdown()?;
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
                self.shutdown()?;
                Err(ProcessError::ExitTimeout { timeout })
            }
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

    fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, ProcessError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_wait()? {
                Some(status) => return Ok(Some(status)),
                None if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
                None => return Ok(None),
            }
        }
    }

    fn finish_reap(&mut self, status: ExitStatus) -> Result<(), ProcessError> {
        let child_id = self.id();
        self.child.take();
        self.join_readers()?;
        if let Some(sender) = self.reap_sender.take() {
            let _ = sender.send(Ok(ReapedChild { child_id, status }));
        }
        Ok(())
    }

    fn join_readers(&mut self) -> Result<(), ProcessError> {
        let deadline = Instant::now() + self.cleanup_timeout;
        loop {
            let stdout_finished = self
                .stdout_reader
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            let stderr_finished = self
                .stderr_reader
                .as_ref()
                .is_none_or(JoinHandle::is_finished);
            match (
                stdout_finished && stderr_finished,
                Instant::now() < deadline,
            ) {
                (true, _) => break,
                (false, true) => std::thread::sleep(POLL_INTERVAL),
                (false, false) => {
                    return Err(ProcessError::ReaderTimeout {
                        timeout: self.cleanup_timeout,
                    });
                }
            }
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

pub fn run_isolated_exact(
    test_name: &str,
    mode: &str,
    assertion_marker: &str,
    timeout: Duration,
) -> Result<IsolatedRun, ProcessError> {
    let mut child = ChildGuard::spawn_exact_current(test_name, mode, timeout)?;
    child.wait_for_readiness(assertion_marker, timeout)?;
    let status = child.wait_bounded(timeout)?;
    Ok(IsolatedRun {
        status,
        stdout: std::mem::take(&mut child.stdout),
        stderr: std::mem::take(&mut child.stderr),
    })
}

pub fn private_child_parent_id() -> Option<u32> {
    std::env::var(CHILD_PARENT_ID_ENV).ok()?.parse().ok()
}

pub fn run_signal_contract_exact(
    test_name: &str,
    mode: &str,
    assertion_marker: &str,
    timeout: Duration,
) -> Result<IsolatedRun, ProcessError> {
    run_isolated_exact(test_name, mode, assertion_marker, timeout)
}
