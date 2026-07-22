use super::{FixtureError, unique};
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub const CAPTURE_LIMIT: usize = 64 * 1024;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_READY_LINE: usize = 4096;

#[derive(Debug)]
pub struct CapturedOutput {
    pub stdout: Box<[u8]>,
    pub stderr: Box<[u8]>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

#[derive(Default)]
struct CaptureState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Clone, Copy, Debug)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct ReaderTerminal {
    kind: StreamKind,
    error: Option<Box<str>>,
}

struct ReadinessBuffer {
    line: Vec<u8>,
    overflowed: bool,
    sent: bool,
}

impl ReadinessBuffer {
    fn new() -> Self {
        Self {
            line: Vec::new(),
            overflowed: false,
            sent: false,
        }
    }

    fn observe(
        &mut self,
        byte: u8,
        readiness: Option<&ReadinessLine>,
        ready: &mpsc::SyncSender<Box<[u8]>>,
    ) {
        if byte == b'\n' {
            self.complete_line(readiness, ready);
            return;
        }
        if self.overflowed {
            return;
        }
        if self.line.len() == MAX_READY_LINE {
            self.line.clear();
            self.overflowed = true;
            return;
        }
        self.line.push(byte);
    }

    fn complete_line(
        &mut self,
        readiness: Option<&ReadinessLine>,
        ready: &mpsc::SyncSender<Box<[u8]>>,
    ) {
        if !self.overflowed && !self.sent {
            self.send_readiness(readiness, ready);
        }
        self.line.clear();
        self.overflowed = false;
    }

    fn send_readiness(
        &mut self,
        readiness: Option<&ReadinessLine>,
        ready: &mpsc::SyncSender<Box<[u8]>>,
    ) {
        let Some(event) = readiness_event(&self.line, readiness) else {
            return;
        };
        let _ = ready.try_send(event);
        self.sent = true;
    }
}

pub struct ChildGuard {
    child: Option<Child>,
    capture: Arc<Mutex<CaptureState>>,
    ready: Option<mpsc::Receiver<Box<[u8]>>>,
    reader_done: mpsc::Receiver<ReaderTerminal>,
    reader_threads: Vec<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
pub(crate) struct ReadinessLine {
    pub(crate) expected: Option<Arc<[u8]>>,
    pub(crate) scan_stderr: bool,
}

impl ChildGuard {
    pub fn spawn(command: &mut Command) -> Result<Self, FixtureError> {
        Self::spawn_inner(command, None)
    }

    pub fn spawn_ready(command: &mut Command, ready_line: &str) -> Result<Self, FixtureError> {
        Self::spawn_output_ready(
            command,
            ReadinessLine {
                expected: Some(Arc::from(ready_line.as_bytes())),
                scan_stderr: true,
            },
        )
    }

    pub(crate) fn spawn_output_ready(
        command: &mut Command,
        readiness: ReadinessLine,
    ) -> Result<Self, FixtureError> {
        Self::spawn_inner(command, Some(readiness))
    }

    pub fn id(&self) -> u32 {
        self.child.as_ref().map_or(0, Child::id)
    }

    pub fn wait_for_ready(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        self.receive_readiness(timeout).map(|_| ())
    }

    pub fn wait_for_tcp(
        &mut self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> Result<(), FixtureError> {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(_) => Ok(()),
            Err(error) => {
                let output = self.output_description();
                self.terminate()?;
                Err(FixtureError::new(format!(
                    "TCP readiness failed for {addr}: {error}; {output}"
                )))
            }
        }
    }

    pub fn terminate(&mut self) -> Result<(), FixtureError> {
        if self.child.is_none() {
            return self.finish_readers(CLEANUP_TIMEOUT);
        }
        let status = match self.poll_exit()? {
            Some(status) => status,
            None => self.kill_running_child()?,
        };
        self.finish_reaped(status, CLEANUP_TIMEOUT)
    }

    pub fn wait(&mut self, timeout: Duration) -> Result<ExitStatus, FixtureError> {
        let deadline = match deadline_after(timeout) {
            Ok(deadline) => deadline,
            Err(error) => {
                self.terminate()?;
                return Err(error);
            }
        };
        loop {
            let status = self.poll_exit()?;
            if let Some(status) = status {
                self.finish_reaped(status, CLEANUP_TIMEOUT)?;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let output = self.output_description();
                self.terminate()?;
                return Err(FixtureError::new(format!("child wait timed out; {output}")));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn poll_exit(&mut self) -> Result<Option<ExitStatus>, FixtureError> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| FixtureError::new("child was already reaped"))?;
        match child.try_wait() {
            Ok(status) => Ok(status),
            Err(_) => abort_cleanup(),
        }
    }

    fn kill_running_child(&mut self) -> Result<ExitStatus, FixtureError> {
        let kill_result = self.child.as_mut().map(Child::kill);
        match kill_result {
            Some(Ok(())) => Ok(self.wait_for_exit(CLEANUP_TIMEOUT)),
            Some(Err(error)) => self.finish_after_kill_failure(error),
            None => abort_cleanup(),
        }
    }

    fn finish_after_kill_failure(
        &mut self,
        error: std::io::Error,
    ) -> Result<ExitStatus, FixtureError> {
        let status = match self.child.as_mut().map(Child::try_wait) {
            Some(Ok(Some(status))) => status,
            Some(Ok(None)) | Some(Err(_)) | None => abort_cleanup(),
        };
        self.finish_reaped(status, CLEANUP_TIMEOUT)?;
        Err(FixtureError::new(format!(
            "failed to kill child after it exited: {error}"
        )))
    }

    pub fn captured_output(&self) -> Result<CapturedOutput, FixtureError> {
        let capture = self
            .capture
            .lock()
            .map_err(|_| FixtureError::new("child output capture lock was poisoned"))?;
        Ok(CapturedOutput {
            stdout: capture.stdout.clone().into_boxed_slice(),
            stderr: capture.stderr.clone().into_boxed_slice(),
            stdout_truncated: capture.stdout_truncated,
            stderr_truncated: capture.stderr_truncated,
        })
    }

    fn spawn_inner(
        command: &mut Command,
        readiness: Option<ReadinessLine>,
    ) -> Result<Self, FixtureError> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| FixtureError::new(format!("failed to spawn child: {error}")))?;
        let stdout = child.stdout.take().unwrap_or_else(|| abort_cleanup());
        let stderr = child.stderr.take().unwrap_or_else(|| abort_cleanup());
        let capture = Arc::new(Mutex::new(CaptureState::default()));
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(2);
        let mut guard = Self {
            child: Some(child),
            capture: Arc::clone(&capture),
            ready: readiness.as_ref().map(|_| ready_rx),
            reader_done: done_rx,
            reader_threads: Vec::with_capacity(2),
        };
        let stdout_thread = spawn_reader(
            "camber-bench-child-stdout",
            stdout,
            StreamKind::Stdout,
            Arc::clone(&capture),
            readiness.clone(),
            ready_tx.clone(),
            done_tx.clone(),
        );
        match stdout_thread {
            Ok(thread) => guard.reader_threads.push(Some(thread)),
            Err(error) => {
                drop(stderr);
                guard.terminate()?;
                return Err(error);
            }
        }
        match spawn_reader(
            "camber-bench-child-stderr",
            stderr,
            StreamKind::Stderr,
            capture,
            stderr_readiness(&readiness),
            ready_tx,
            done_tx,
        ) {
            Ok(thread) => guard.reader_threads.push(Some(thread)),
            Err(error) => {
                guard.terminate()?;
                return Err(error);
            }
        }
        Ok(guard)
    }

    pub(crate) fn receive_readiness(
        &mut self,
        timeout: Duration,
    ) -> Result<Box<[u8]>, FixtureError> {
        let readiness = match self.ready.as_ref() {
            Some(ready) => ready.recv_timeout(timeout),
            None => return self.fail_readiness("child has no readiness configuration"),
        };
        match readiness {
            Ok(event) => Ok(event),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.fail_readiness("readiness timed out waiting for child output")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.fail_readiness("readiness output ended before child became ready")
            }
        }
    }

    fn fail_readiness<T>(&mut self, message: &str) -> Result<T, FixtureError> {
        self.terminate()?;
        let output = self.output_description();
        Err(FixtureError::new(format!("{message}; {output}")))
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(|| abort_cleanup());
        loop {
            match self.child.as_mut().and_then(|child| child.try_wait().ok()) {
                Some(Some(status)) => return status,
                Some(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
                Some(None) | None => abort_cleanup(),
            }
        }
    }

    fn finish_reaped(&mut self, status: ExitStatus, timeout: Duration) -> Result<(), FixtureError> {
        let _ = status;
        self.child = None;
        self.finish_readers(timeout)
    }

    fn finish_readers(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        if self.reader_threads.is_empty() {
            return Ok(());
        }
        let deadline = deadline_after(timeout).unwrap_or_else(|_| abort_cleanup());
        let mut terminals = Vec::with_capacity(self.reader_threads.len());
        while terminals.len() < self.reader_threads.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.reader_done.recv_timeout(remaining) {
                Ok(terminal) => terminals.push(terminal),
                Err(_) => abort_cleanup(),
            }
        }
        wait_for_threads(&self.reader_threads, deadline);
        for thread in &mut self.reader_threads {
            let Some(thread) = thread.take() else {
                continue;
            };
            if thread.join().is_err() {
                abort_cleanup();
            }
        }
        self.reader_threads.clear();
        match terminals.into_iter().find_map(|terminal| {
            terminal
                .error
                .map(|error| format!("{:?} reader failed: {error}", terminal.kind))
        }) {
            Some(error) => Err(FixtureError::new(error)),
            None => Ok(()),
        }
    }

    fn output_description(&self) -> Box<str> {
        match self.captured_output() {
            Ok(output) => format!(
                "stdout={:?}, stderr={:?}, stdout_truncated={}, stderr_truncated={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                output.stdout_truncated,
                output.stderr_truncated
            )
            .into_boxed_str(),
            Err(error) => format!("output unavailable: {error}").into_boxed_str(),
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.terminate().is_err() {
            abort_cleanup();
        }
    }
}

pub struct ServerProcess {
    child: ChildGuard,
    pub port: u16,
}

impl ServerProcess {
    pub fn spawn(binary: &str, args: &[&str]) -> Result<Self, FixtureError> {
        let mut last_error = None;
        for attempt in 0..8 {
            let port = candidate_port(attempt);
            let mut command = Command::new(binary);
            command.args(args).args(["--port", &port.to_string()]);
            let mut child = ChildGuard::spawn_ready(&mut command, "ready")?;
            match child.wait_for_ready(SERVER_START_TIMEOUT) {
                Ok(()) => {
                    let addr = SocketAddr::from(([127, 0, 0, 1], port));
                    child.wait_for_tcp(addr, SERVER_START_TIMEOUT)?;
                    return Ok(Self { child, port });
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| FixtureError::new("server failed to start")))
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }
}

pub fn spawn_current_test_child(
    test_name: &str,
    child_environment: &str,
    child_value: &str,
) -> Result<ChildGuard, FixtureError> {
    spawn_test_child(test_name, child_environment, child_value, None)
}

pub fn spawn_current_test_ready_child(
    test_name: &str,
    child_environment: &str,
    child_value: &str,
    ready_line: &str,
) -> Result<ChildGuard, FixtureError> {
    spawn_test_child(
        test_name,
        child_environment,
        child_value,
        Some(ReadinessLine {
            expected: Some(Arc::from(ready_line.as_bytes())),
            scan_stderr: true,
        }),
    )
}

pub fn spawn_current_test(test_name: &str) -> Result<ChildGuard, FixtureError> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command.args(["--exact", test_name, "--nocapture"]);
    ChildGuard::spawn(&mut command)
}

#[cfg(unix)]
pub fn process_exists(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
pub fn process_exists(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

fn spawn_test_child(
    test_name: &str,
    child_environment: &str,
    child_value: &str,
    readiness: Option<ReadinessLine>,
) -> Result<ChildGuard, FixtureError> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .args(["--exact", test_name, "--nocapture"])
        .env(child_environment, child_value);
    match readiness {
        Some(readiness) => ChildGuard::spawn_inner(&mut command, Some(readiness)),
        None => ChildGuard::spawn(&mut command),
    }
}

fn spawn_reader<Output>(
    thread_name: &str,
    output: Output,
    kind: StreamKind,
    capture: Arc<Mutex<CaptureState>>,
    readiness: Option<ReadinessLine>,
    ready: mpsc::SyncSender<Box<[u8]>>,
    done: mpsc::SyncSender<ReaderTerminal>,
) -> Result<JoinHandle<()>, FixtureError>
where
    Output: Read + Send + 'static,
{
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drain_output(output, kind, &capture, readiness.as_ref(), &ready)
            }));
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string().into_boxed_str()),
                Err(_) => Some("reader panicked".into()),
            };
            let _ = done.send(ReaderTerminal { kind, error });
        })
        .map_err(|error| FixtureError::new(format!("failed to spawn {kind:?} reader: {error}")))
}

fn drain_output<Output: Read>(
    mut output: Output,
    kind: StreamKind,
    capture: &Mutex<CaptureState>,
    readiness: Option<&ReadinessLine>,
    ready: &mpsc::SyncSender<Box<[u8]>>,
) -> Result<(), FixtureError> {
    let mut buffer = [0_u8; 8192];
    let mut readiness_buffer = ReadinessBuffer::new();
    loop {
        let read = output.read(&mut buffer)?;
        if read == 0 {
            return Ok(());
        }
        append_capture(capture, kind, &buffer[..read])?;
        for byte in &buffer[..read] {
            readiness_buffer.observe(*byte, readiness, ready);
        }
    }
}

fn wait_for_threads(threads: &[Option<JoinHandle<()>>], deadline: Instant) {
    loop {
        if threads
            .iter()
            .all(|thread| thread.as_ref().is_none_or(JoinHandle::is_finished))
        {
            return;
        }
        if Instant::now() >= deadline {
            abort_cleanup();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn append_capture(
    capture: &Mutex<CaptureState>,
    kind: StreamKind,
    bytes: &[u8],
) -> Result<(), FixtureError> {
    let mut capture = capture
        .lock()
        .map_err(|_| FixtureError::new("child output capture lock was poisoned"))?;
    match kind {
        StreamKind::Stdout => {
            let truncated = append_bounded(&mut capture.stdout, bytes);
            capture.stdout_truncated |= truncated;
        }
        StreamKind::Stderr => {
            let truncated = append_bounded(&mut capture.stderr, bytes);
            capture.stderr_truncated |= truncated;
        }
    }
    Ok(())
}

fn append_bounded(output: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let remaining = CAPTURE_LIMIT.saturating_sub(output.len());
    let retained = remaining.min(bytes.len());
    output.extend_from_slice(&bytes[..retained]);
    retained < bytes.len()
}

fn stderr_readiness(readiness: &Option<ReadinessLine>) -> Option<ReadinessLine> {
    match readiness {
        Some(readiness) if readiness.scan_stderr => Some(readiness.clone()),
        Some(_) | None => None,
    }
}

fn readiness_event(line: &[u8], readiness: Option<&ReadinessLine>) -> Option<Box<[u8]>> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    match readiness {
        Some(ReadinessLine {
            expected: Some(expected),
            ..
        }) if line == expected.as_ref() => Some(line.into()),
        Some(ReadinessLine { expected: None, .. })
            if std::str::from_utf8(line)
                .ok()
                .and_then(|line| line.parse::<SocketAddr>().ok())
                .is_some() =>
        {
            Some(line.into())
        }
        Some(_) | None => None,
    }
}

fn candidate_port(attempt: u16) -> u16 {
    let unique = unique::name("port");
    let hash = unique.bytes().fold(0_u16, |value, byte| {
        value.wrapping_mul(31).wrapping_add(u16::from(byte))
    });
    20_000 + hash.wrapping_add(attempt) % 40_000
}

fn deadline_after(timeout: Duration) -> Result<Instant, FixtureError> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| FixtureError::new("timeout deadline overflow"))
}

fn abort_cleanup() -> ! {
    std::process::abort()
}
