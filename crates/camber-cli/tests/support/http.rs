use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const COOPERATIVE_IO_SLICE: Duration = Duration::from_millis(25);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEADER_SIZE: usize = 64 * 1024;
const MAX_BODY_SIZE: usize = 1024 * 1024;

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Box<str>,
}

pub fn connect_tcp(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

pub fn connect_unix(path: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

pub fn request_tcp(
    addr: SocketAddr,
    method: &str,
    host: &str,
    path: &str,
) -> io::Result<HttpResponse> {
    let mut stream = connect_tcp(addr)?;
    write_request(&mut stream, method, host, path, true)?;
    read_response(&mut stream)
}

pub fn request_unix(
    path: &Path,
    method: &str,
    host: &str,
    request_path: &str,
) -> io::Result<HttpResponse> {
    let mut stream = connect_unix(path)?;
    write_request(&mut stream, method, host, request_path, true)?;
    read_response(&mut stream)
}

pub fn write_request<W: Write>(
    stream: &mut W,
    method: &str,
    host: &str,
    path: &str,
    close: bool,
) -> io::Result<()> {
    let connection = match close {
        true => "close",
        false => "keep-alive",
    };
    let request =
        format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: {connection}\r\n\r\n");
    stream.write_all(request.as_bytes())
}

pub fn read_response<R: Read>(stream: &mut R) -> io::Result<HttpResponse> {
    let mut bytes = Vec::new();
    let header_end = read_headers(stream, &mut bytes)?;
    let content_length = response_content_length(&bytes[..header_end])?;
    match content_length {
        Some(length) if length <= MAX_BODY_SIZE => {
            let expected = header_end
                .checked_add(length)
                .ok_or_else(|| invalid_data("response length overflowed"))?;
            read_body(stream, &mut bytes, expected)?;
        }
        Some(_) => return Err(invalid_data("response body exceeded size limit")),
        None => {
            let maximum = header_end
                .checked_add(MAX_BODY_SIZE)
                .ok_or_else(|| invalid_data("response length overflowed"))?;
            read_body_to_end(stream, &mut bytes, maximum)?;
        }
    }
    parse_response(&bytes)
}

fn read_headers<R: Read>(stream: &mut R, bytes: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        match header_end(bytes)? {
            Some(end) => return Ok(end),
            None => {}
        }
        let mut chunk = [0_u8; 1024];
        let remaining = remaining_capacity(
            MAX_HEADER_SIZE,
            bytes.len(),
            "response headers exceeded size limit",
        )?;
        let read_limit = remaining.min(chunk.len());
        match stream.read(&mut chunk[..read_limit])? {
            0 => return Err(invalid_data("response ended before headers completed")),
            count => append_with_limit(
                bytes,
                &chunk[..count],
                MAX_HEADER_SIZE,
                "response headers exceeded size limit",
            )?,
        }
    }
}

fn read_body<R: Read>(stream: &mut R, bytes: &mut Vec<u8>, expected: usize) -> io::Result<()> {
    while bytes.len() < expected {
        let remaining = expected
            .checked_sub(bytes.len())
            .ok_or_else(|| invalid_data("response body exceeded declared length"))?;
        let mut chunk = [0_u8; 1024];
        let read_limit = remaining.min(chunk.len());
        match stream.read(&mut chunk[..read_limit])? {
            0 => return Err(invalid_data("response ended before body completed")),
            count => append_with_limit(
                bytes,
                &chunk[..count],
                expected,
                "response body exceeded declared length",
            )?,
        }
    }
    Ok(())
}

fn read_body_to_end<R: Read>(
    stream: &mut R,
    bytes: &mut Vec<u8>,
    maximum: usize,
) -> io::Result<()> {
    loop {
        let remaining = maximum
            .checked_sub(bytes.len())
            .ok_or_else(|| invalid_data("response body exceeded size limit"))?;
        if remaining == 0 {
            return verify_response_end(stream);
        }
        let mut chunk = [0_u8; 1024];
        let read_limit = remaining.min(chunk.len());
        match stream.read(&mut chunk[..read_limit])? {
            0 => return Ok(()),
            count => append_with_limit(
                bytes,
                &chunk[..count],
                maximum,
                "response body exceeded size limit",
            )?,
        }
    }
}

fn verify_response_end<R: Read>(stream: &mut R) -> io::Result<()> {
    let mut extra = [0_u8; 1];
    match stream.read(&mut extra)? {
        0 => Ok(()),
        _ => Err(invalid_data("response body exceeded size limit")),
    }
}

fn header_end(bytes: &[u8]) -> io::Result<Option<usize>> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| {
            position
                .checked_add(4)
                .ok_or_else(|| invalid_data("header length overflowed"))
        })
        .transpose()
}

fn response_content_length(headers: &[u8]) -> io::Result<Option<usize>> {
    let headers = std::str::from_utf8(headers)
        .map_err(|error| invalid_data(format!("response headers were not UTF-8: {error}")))?;
    headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then_some(value.trim())
        })
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|error| invalid_data(format!("invalid response content length: {error}")))
}

fn parse_response(bytes: &[u8]) -> io::Result<HttpResponse> {
    let header_end = header_end(bytes)?
        .ok_or_else(|| invalid_data("response did not contain complete headers"))?;
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| invalid_data(format!("response headers were not UTF-8: {error}")))?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| invalid_data("response did not contain a valid status"))?;
    let content_length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    let body_bytes = &bytes[header_end..];
    match content_length {
        Some(expected) if body_bytes.len() != expected => {
            return Err(invalid_data(format!(
                "response body length was {}, expected {expected}",
                body_bytes.len()
            )));
        }
        Some(_) | None => {}
    }
    let body = std::str::from_utf8(body_bytes)
        .map_err(|error| invalid_data(format!("response body was not UTF-8: {error}")))?;
    Ok(HttpResponse {
        status,
        body: body.into(),
    })
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn remaining_capacity(maximum: usize, current: usize, message: &str) -> io::Result<usize> {
    match maximum.checked_sub(current) {
        Some(0) | None => Err(invalid_data(message)),
        Some(remaining) => Ok(remaining),
    }
}

fn append_with_limit(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
    maximum: usize,
    message: &str,
) -> io::Result<()> {
    let new_length = bytes
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| invalid_data("buffer length overflowed"))?;
    match new_length <= maximum {
        true => {
            bytes.extend_from_slice(chunk);
            Ok(())
        }
        false => Err(invalid_data(message)),
    }
}

pub struct Backend {
    addr: SocketAddr,
    stop: Sender<()>,
    completion: Receiver<io::Result<BackendReport>>,
    thread: Option<JoinHandle<()>>,
    join_sender: Option<Sender<BackendShutdown>>,
    join_receiver: Option<Receiver<BackendShutdown>>,
    expected_requests: usize,
    completion_timeout: Duration,
}

#[derive(Debug)]
pub struct BackendReport {
    request_paths: Box<[Box<str>]>,
}

impl BackendReport {
    pub fn request_paths(&self) -> impl Iterator<Item = &str> {
        self.request_paths.iter().map(AsRef::as_ref)
    }

    fn request_count(&self) -> usize {
        self.request_paths.len()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(
        "backend completed {completed_requests} requests before timeout; expected {expected_requests}"
    )]
    CompletionTimeout {
        expected_requests: usize,
        completed_requests: usize,
    },
    #[error("backend request failed: {0}")]
    Request(#[source] io::Error),
    #[error("backend worker panicked")]
    WorkerPanicked,
    #[error("backend worker did not stop before teardown deadline")]
    TeardownTimeout,
    #[error("backend worker ownership was already consumed")]
    WorkerNotOwned,
}

pub struct BackendJoinProbe {
    receiver: Receiver<BackendShutdown>,
}

impl BackendJoinProbe {
    pub fn wait(self) -> Result<BackendShutdown, Box<str>> {
        self.receiver
            .recv_timeout(TEARDOWN_TIMEOUT)
            .map_err(|error| format!("wait for backend join completion: {error}").into_boxed_str())
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BackendShutdown {
    ListenerReleasedAndWorkerJoined,
}

impl Backend {
    pub fn one(body: &'static str) -> Self {
        Self::start(200, body, 1, TEARDOWN_TIMEOUT)
    }

    pub fn one_with_completion_timeout(body: &'static str, timeout: Duration) -> Self {
        Self::start(200, body, 1, timeout)
    }

    pub fn many(body: &'static str, expected_requests: usize) -> Self {
        Self::start(200, body, expected_requests, TEARDOWN_TIMEOUT)
    }

    pub fn unhealthy() -> Self {
        Self::start(500, "unhealthy", 1, TEARDOWN_TIMEOUT)
    }

    fn start(
        status: u16,
        body: &'static str,
        expected_requests: usize,
        completion_timeout: Duration,
    ) -> Self {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(_) => std::process::abort(),
        };
        let addr = match listener.local_addr() {
            Ok(addr) => addr,
            Err(_) => std::process::abort(),
        };
        match listener.set_nonblocking(true) {
            Ok(()) => {}
            Err(_) => std::process::abort(),
        }
        let (stop_sender, stop_receiver) = mpsc::channel();
        let (completion_sender, completion_receiver) = mpsc::channel();
        let (join_sender, join_receiver) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = serve(listener, status, body, expected_requests, &stop_receiver);
            let _ = completion_sender.send(result);
        });
        Self {
            addr,
            stop: stop_sender,
            completion: completion_receiver,
            thread: Some(thread),
            join_sender: Some(join_sender),
            join_receiver: Some(join_receiver),
            expected_requests,
            completion_timeout,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn take_join_probe(&mut self) -> Option<BackendJoinProbe> {
        self.join_receiver
            .take()
            .map(|receiver| BackendJoinProbe { receiver })
    }

    pub fn finish(mut self) -> Result<BackendReport, BackendError> {
        match self.completion.recv_timeout(self.completion_timeout) {
            Ok(result) => self.join_with_result(result),
            Err(RecvTimeoutError::Timeout) => self.finish_after_timeout(),
            Err(RecvTimeoutError::Disconnected) => self.join_disconnected_worker(),
        }
    }

    pub fn stop(mut self) -> Result<BackendReport, BackendError> {
        self.request_stop();
        let result = self.receive_stopped_result();
        self.join_with_result(result)
    }

    fn finish_after_timeout(&mut self) -> Result<BackendReport, BackendError> {
        self.request_stop();
        let result = self.receive_stopped_result();
        let report = self.join_with_result(result)?;
        Err(BackendError::CompletionTimeout {
            expected_requests: self.expected_requests,
            completed_requests: report.request_count(),
        })
    }

    fn request_stop(&self) {
        let _ = self.stop.send(());
    }

    fn receive_stopped_result(&mut self) -> io::Result<BackendReport> {
        match self.completion.recv_timeout(TEARDOWN_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("backend completion channel disconnected"))
            }
            Err(RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "backend worker did not report completion before teardown deadline",
            )),
        }
    }

    fn join_with_result(
        &mut self,
        result: io::Result<BackendReport>,
    ) -> Result<BackendReport, BackendError> {
        self.join_worker()?;
        result.map_err(BackendError::Request)
    }

    fn join_disconnected_worker(&mut self) -> Result<BackendReport, BackendError> {
        self.join_worker()?;
        Err(BackendError::WorkerPanicked)
    }

    fn join_worker(&mut self) -> Result<(), BackendError> {
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        loop {
            match self.thread.as_ref() {
                Some(thread) if thread.is_finished() => break,
                Some(_) if Instant::now() < deadline => std::thread::yield_now(),
                Some(_) => return Err(BackendError::TeardownTimeout),
                None => return Err(BackendError::WorkerNotOwned),
            }
        }
        let thread = match self.thread.take() {
            Some(thread) => thread,
            None => return Err(BackendError::WorkerNotOwned),
        };
        let join_result = thread.join().map_err(|_| BackendError::WorkerPanicked);
        self.report_joined();
        join_result
    }

    fn report_joined(&mut self) {
        match self.join_sender.take() {
            Some(sender) => drop(sender.send(BackendShutdown::ListenerReleasedAndWorkerJoined)),
            None => {}
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        if self.thread.is_some() && self.cleanup_for_drop().is_err() {
            std::process::abort();
        }
    }
}

impl Backend {
    fn cleanup_for_drop(&mut self) -> Result<(), BackendError> {
        self.request_stop();
        match self.completion.recv_timeout(TEARDOWN_TIMEOUT) {
            Ok(_) | Err(RecvTimeoutError::Disconnected) => self.join_worker(),
            Err(RecvTimeoutError::Timeout) => Err(BackendError::TeardownTimeout),
        }
    }
}

enum ResponseOutcome {
    Completed(Box<str>),
    Stopped,
}

fn serve(
    listener: TcpListener,
    status: u16,
    body: &'static str,
    expected_requests: usize,
    stop: &Receiver<()>,
) -> io::Result<BackendReport> {
    let mut request_paths = Vec::with_capacity(expected_requests);
    while request_paths.len() < expected_requests {
        match serve_one(&listener, status, body, stop)? {
            ResponseOutcome::Completed(path) => request_paths.push(path),
            ResponseOutcome::Stopped => return Ok(report(request_paths)),
        }
    }
    Ok(report(request_paths))
}

fn serve_one(
    listener: &TcpListener,
    status: u16,
    body: &str,
    stop: &Receiver<()>,
) -> io::Result<ResponseOutcome> {
    let Some(mut stream) = accept(listener, stop)? else {
        return Ok(ResponseOutcome::Stopped);
    };
    respond(&mut stream, status, body, stop)
}

fn accept(listener: &TcpListener, stop: &Receiver<()>) -> io::Result<Option<TcpStream>> {
    loop {
        let accept_result = listener.accept();
        match handle_accept_result(accept_result, stop)? {
            Some(stream) => return Ok(stream),
            None => {}
        }
    }
}

fn handle_accept_result(
    result: io::Result<(TcpStream, SocketAddr)>,
    stop: &Receiver<()>,
) -> io::Result<Option<Option<TcpStream>>> {
    match result {
        Ok((stream, _)) => Ok(Some(Some(stream))),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(retry_accept(stop)),
        Err(error) => Err(error),
    }
}

fn retry_accept(stop: &Receiver<()>) -> Option<Option<TcpStream>> {
    match stop.recv_timeout(COOPERATIVE_IO_SLICE) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => Some(None),
        Err(RecvTimeoutError::Timeout) => None,
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    body: &str,
    stop: &Receiver<()>,
) -> io::Result<ResponseOutcome> {
    stream.set_read_timeout(Some(COOPERATIVE_IO_SLICE))?;
    stream.set_write_timeout(Some(COOPERATIVE_IO_SLICE))?;
    let request_path = match read_request_path(stream, stop)? {
        Some(path) => path,
        None => return Ok(ResponseOutcome::Stopped),
    };
    let reason = match status {
        200 => "OK",
        500 => "Internal Server Error",
        _ => "Response",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    match write_all_cooperatively(stream, response.as_bytes(), stop)? {
        true => Ok(ResponseOutcome::Completed(request_path)),
        false => Ok(ResponseOutcome::Stopped),
    }
}

fn read_request_path(stream: &mut TcpStream, stop: &Receiver<()>) -> io::Result<Option<Box<str>>> {
    let mut request = Vec::new();
    let header_end = match read_tcp_headers(stream, &mut request, stop)? {
        Some(end) => end,
        None => return Ok(None),
    };
    let headers = std::str::from_utf8(&request[..header_end])
        .map_err(|error| invalid_data(format!("backend request was not UTF-8: {error}")))?;
    headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|path| Some(path.into()))
        .ok_or_else(|| invalid_data("backend request did not contain a target"))
}

fn read_tcp_headers(
    stream: &mut TcpStream,
    request: &mut Vec<u8>,
    stop: &Receiver<()>,
) -> io::Result<Option<usize>> {
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        match header_end(request)? {
            Some(end) => return Ok(Some(end)),
            None => {}
        }
        if stop_requested(stop) {
            return Ok(None);
        }
        let mut chunk = [0_u8; 1024];
        let remaining = remaining_capacity(
            MAX_HEADER_SIZE,
            request.len(),
            "backend request headers exceeded size limit",
        )?;
        let read_limit = remaining.min(chunk.len());
        match stream.read(&mut chunk[..read_limit]) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            Ok(count) => append_with_limit(
                request,
                &chunk[..count],
                MAX_HEADER_SIZE,
                "backend request headers exceeded size limit",
            )?,
            Err(error) if is_retryable_timeout(&error) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
}

fn write_all_cooperatively(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    stop: &Receiver<()>,
) -> io::Result<bool> {
    let deadline = Instant::now() + IO_TIMEOUT;
    while !bytes.is_empty() {
        if stop_requested(stop) {
            return Ok(false);
        }
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(count) => bytes = &bytes[count..],
            Err(error) if is_retryable_timeout(&error) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

fn stop_requested(stop: &Receiver<()>) -> bool {
    matches!(stop.try_recv(), Ok(()) | Err(TryRecvError::Disconnected))
}

fn is_retryable_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn report(request_paths: Vec<Box<str>>) -> BackendReport {
    BackendReport {
        request_paths: request_paths.into_boxed_slice(),
    }
}
