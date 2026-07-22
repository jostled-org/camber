use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{self, Router, ServerHandle};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_HEADER_BYTES + MAX_BODY_BYTES + MAX_HEADER_BYTES;
const SERVER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("fixture I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("fixture runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
    #[error("server did not return a valid HTTP readiness response before {timeout:?}")]
    ReadinessTimeout { timeout: Duration },
    #[error("server shutdown did not complete before {timeout:?}")]
    ShutdownTimeout { timeout: Duration },
}

pub struct BoundListener {
    listener: std::net::TcpListener,
    local_addr: SocketAddr,
}

impl BoundListener {
    pub fn bind_tcp(addr: &str) -> Result<Self, io::Error> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn into_tokio(self) -> Result<tokio::net::TcpListener, io::Error> {
        tokio::net::TcpListener::from_std(self.listener)
    }
}

#[derive(Default)]
struct ServerCleanupState {
    joined: std::sync::atomic::AtomicBool,
    error: Mutex<Option<Box<str>>>,
}

pub struct ServerCleanupProbe(Arc<ServerCleanupState>);

impl ServerCleanupProbe {
    pub fn joined(&self) -> bool {
        self.0.joined.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn cleanup_error(&self) -> Option<Box<str>> {
        self.0
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

pub struct ReadyServer {
    local_addr: SocketAddr,
    handle: Option<ServerHandle>,
    readiness: HttpResponse,
    cleanup: Arc<ServerCleanupState>,
}

impl ReadyServer {
    pub fn start(
        listener: BoundListener,
        router: Router,
        timeout: Duration,
    ) -> Result<Self, FixtureError> {
        let local_addr = listener.local_addr();
        let handle = http::serve_background(listener.into_tokio()?, router);
        let cleanup = Arc::new(ServerCleanupState::default());
        let readiness = match wait_for_http_response(local_addr, timeout) {
            Ok(response) => response,
            Err(_) => {
                let mut server = Self {
                    local_addr,
                    handle: Some(handle),
                    readiness: HttpResponse::empty(),
                    cleanup,
                };
                server.cancel_and_join(SERVER_CLEANUP_TIMEOUT)?;
                return Err(FixtureError::ReadinessTimeout { timeout });
            }
        };
        Ok(Self {
            local_addr,
            handle: Some(handle),
            readiness,
            cleanup,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn readiness_response(&self) -> &HttpResponse {
        &self.readiness
    }

    pub fn cleanup_probe(&self) -> ServerCleanupProbe {
        ServerCleanupProbe(Arc::clone(&self.cleanup))
    }

    pub fn shutdown_bounded(mut self, timeout: Duration) -> Result<(), FixtureError> {
        self.shutdown_and_join(timeout)
    }

    fn shutdown_and_join(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return Ok(()),
        };
        handle.shutdown();
        self.join(handle, timeout)
    }

    fn cancel_and_join(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return Ok(()),
        };
        handle.cancel();
        self.join(handle, timeout)
    }

    fn join(&self, handle: ServerHandle, timeout: Duration) -> Result<(), FixtureError> {
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(tokio::time::timeout(timeout, handle.join()))
        });
        match result {
            Ok(Ok(())) | Ok(Err(RuntimeError::Cancelled)) => {
                self.cleanup
                    .joined
                    .store(true, std::sync::atomic::Ordering::Release);
                Ok(())
            }
            Ok(Err(error)) => Err(FixtureError::Runtime(error)),
            Err(_) => Err(FixtureError::ShutdownTimeout { timeout }),
        }
    }

    fn record_cleanup_error(&self, error: &FixtureError) {
        let mut cleanup_error = self
            .cleanup
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cleanup_error = Some(error.to_string().into_boxed_str());
    }
}

impl Drop for ReadyServer {
    fn drop(&mut self) {
        if let Err(error) = self.cancel_and_join(SERVER_CLEANUP_TIMEOUT) {
            self.record_cleanup_error(&error);
        }
    }
}

pub fn spawn_server_ready(router: Router, timeout: Duration) -> Result<ReadyServer, FixtureError> {
    let listener = BoundListener::bind_tcp("127.0.0.1:0")?;
    ReadyServer::start(listener, router, timeout)
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Box<[(Box<str>, Box<str>)]>,
    pub body: Box<[u8]>,
    raw: Box<[u8]>,
}

impl HttpResponse {
    pub(crate) fn empty() -> Self {
        Self {
            status: 0,
            headers: Box::new([]),
            body: Box::new([]),
            raw: Box::new([]),
        }
    }

    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
    }
}

pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_request(&mut stream, method, path, headers, body)?;
    read_http_response(&mut stream)
}

pub fn wait_for_http_response(addr: SocketAddr, timeout: Duration) -> io::Result<HttpResponse> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "readiness deadline overflowed")
        })?;
    let mut last_error = io::Error::from(io::ErrorKind::TimedOut);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("HTTP readiness timed out; last error: {last_error}"),
            ));
        }
        let attempt = remaining.min(Duration::from_millis(100));
        match probe_transport(addr, attempt) {
            Ok(response) => return Ok(response),
            Err(error) => last_error = error,
        }
        std::thread::yield_now();
    }
}

fn probe_transport(addr: SocketAddr, timeout: Duration) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\n\r\n")?;
    stream.flush()?;
    read_http_response(&mut stream)
}

pub fn raw_request(addr: SocketAddr, method: &str, path: &str, headers: &[(&str, &str)]) -> String {
    raw_request_with_body(addr, method, path, headers, &[])
}

pub fn raw_request_with_body(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> String {
    let mut stream = connect(addr).unwrap();
    write_request(&mut stream, method, path, headers, body).unwrap();
    let response = read_http_response(&mut stream).unwrap();
    String::from_utf8_lossy(response.raw()).into_owned()
}

pub fn status_from_raw(raw: &str) -> u16 {
    raw.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or(0)
}

pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

pub async fn assert_admission_closed(addr: SocketAddr, timeout: Duration) {
    let mut stream = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await
    {
        Err(_) | Ok(Err(_)) => return,
        Ok(Ok(stream)) => stream,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let request = b"GET /retained HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    match stream.write_all(request).await {
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::NotConnected
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed while probing closed admission: {error}"),
        Ok(()) => {}
    }
    let mut byte = [0u8; 1];
    match tokio::time::timeout(timeout, stream.read(&mut byte)).await {
        Ok(Ok(0)) => {}
        Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionReset => {}
        Ok(Ok(read)) => panic!("closed admission produced {read} response byte(s)"),
        Ok(Err(error)) => panic!("failed while waiting for closed admission: {error}"),
        Err(_) => panic!("timed out waiting for closed admission"),
    }
}

pub fn write_request(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    headers.iter().for_each(|(name, value)| {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    });
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

pub fn read_http_response(stream: &mut TcpStream) -> io::Result<HttpResponse> {
    let mut raw = Vec::new();
    let header_end = read_through(stream, &mut raw, b"\r\n\r\n", MAX_HEADER_BYTES)?;
    let (status, headers) = parse_head(&raw[..header_end])?;
    let body: Box<[u8]> = match response_body_kind(status, &headers)? {
        BodyKind::None => Vec::new().into_boxed_slice(),
        BodyKind::Length(length) => {
            let body_end = header_end
                .checked_add(length)
                .ok_or_else(|| invalid_data("response length overflowed"))?;
            read_to_length(stream, &mut raw, body_end, MAX_RESPONSE_BYTES)?;
            raw[header_end..body_end].into()
        }
        BodyKind::Chunked => read_chunked_body(stream, &mut raw, header_end)?,
        BodyKind::Eof => {
            read_to_eof(stream, &mut raw, MAX_RESPONSE_BYTES)?;
            raw[header_end..].into()
        }
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
        raw: raw.into_boxed_slice(),
    })
}

enum BodyKind {
    None,
    Length(usize),
    Chunked,
    Eof,
}

fn response_body_kind(status: u16, headers: &[(Box<str>, Box<str>)]) -> io::Result<BodyKind> {
    if (100..200).contains(&status) || matches!(status, 204 | 304) {
        return Ok(BodyKind::None);
    }
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
    });
    if chunked {
        return Ok(BodyKind::Chunked);
    }
    let lengths = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid_data(format!("invalid content length: {error}")))?;
    match lengths.as_slice() {
        [] => Ok(BodyKind::Eof),
        [length, rest @ ..] if rest.iter().all(|candidate| candidate == length) => {
            validated_body_length(*length)
        }
        _ => Err(invalid_data(
            "response contained conflicting content lengths",
        )),
    }
}

fn validated_body_length(length: usize) -> io::Result<BodyKind> {
    match length <= MAX_BODY_BYTES {
        true => Ok(BodyKind::Length(length)),
        false => Err(invalid_data("response body exceeded size limit")),
    }
}

fn parse_head(bytes: &[u8]) -> io::Result<(u16, Box<[(Box<str>, Box<str>)]>)> {
    let head = std::str::from_utf8(bytes)
        .map_err(|error| invalid_data(format!("response head was not UTF-8: {error}")))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| invalid_data("response did not contain a valid status"))?;
    let headers = lines
        .take_while(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| invalid_data("response contained a malformed header"))?;
            Ok((name.into(), value.trim().into()))
        })
        .collect::<io::Result<Vec<_>>>()?
        .into_boxed_slice();
    Ok((status, headers))
}

fn read_chunked_body(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    mut cursor: usize,
) -> io::Result<Box<[u8]>> {
    let mut body = Vec::new();
    loop {
        let line_end = read_line(stream, raw, cursor)?;
        let size_end = line_end
            .checked_sub(2)
            .ok_or_else(|| invalid_data("chunk size framing underflowed"))?;
        let size_line = std::str::from_utf8(&raw[cursor..size_end])
            .map_err(|error| invalid_data(format!("chunk size was not UTF-8: {error}")))?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or_default(), 16)
            .map_err(|error| invalid_data(format!("invalid chunk size: {error}")))?;
        cursor = line_end;
        if size == 0 {
            read_chunk_trailers(stream, raw, cursor)?;
            return Ok(body.into_boxed_slice());
        }
        let body_end = body
            .len()
            .checked_add(size)
            .ok_or_else(|| invalid_data("chunk body length overflowed"))?;
        if body_end > MAX_BODY_BYTES {
            return Err(invalid_data("response body exceeded size limit"));
        }
        let payload_end = cursor
            .checked_add(size)
            .ok_or_else(|| invalid_data("chunk payload length overflowed"))?;
        let framed_end = payload_end
            .checked_add(2)
            .ok_or_else(|| invalid_data("chunk framing length overflowed"))?;
        read_to_length(stream, raw, framed_end, MAX_RESPONSE_BYTES)?;
        body.extend_from_slice(&raw[cursor..payload_end]);
        if &raw[payload_end..framed_end] != b"\r\n" {
            return Err(invalid_data("chunk did not end with CRLF"));
        }
        cursor = framed_end;
    }
}

fn read_chunk_trailers(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    mut cursor: usize,
) -> io::Result<()> {
    loop {
        let trailer_end = read_line(stream, raw, cursor)?;
        let empty_line_end = cursor
            .checked_add(2)
            .ok_or_else(|| invalid_data("chunk trailer length overflowed"))?;
        cursor = trailer_end;
        if trailer_end == empty_line_end {
            return Ok(());
        }
    }
}

fn read_line(stream: &mut TcpStream, bytes: &mut Vec<u8>, start: usize) -> io::Result<usize> {
    loop {
        if let Some(position) = bytes[start..]
            .windows(2)
            .position(|window| window == b"\r\n")
        {
            return start
                .checked_add(position)
                .and_then(|end| end.checked_add(2))
                .ok_or_else(|| invalid_data("response line length overflowed"));
        }
        if bytes.len() >= MAX_RESPONSE_BYTES {
            return Err(invalid_data("response exceeded size limit"));
        }
        read_one(stream, bytes)?;
    }
}

fn read_through(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    delimiter: &[u8],
    limit: usize,
) -> io::Result<usize> {
    loop {
        if let Some(position) = bytes
            .windows(delimiter.len())
            .position(|part| part == delimiter)
        {
            return position
                .checked_add(delimiter.len())
                .ok_or_else(|| invalid_data("response header length overflowed"));
        }
        if bytes.len() >= limit {
            return Err(invalid_data("response headers exceeded size limit"));
        }
        read_one(stream, bytes)?;
    }
}

fn read_to_length(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    expected: usize,
    limit: usize,
) -> io::Result<()> {
    if expected > limit {
        return Err(invalid_data("response exceeded size limit"));
    }
    while bytes.len() < expected {
        let remaining = expected - bytes.len();
        let mut chunk = [0_u8; 4096];
        let read_limit = remaining.min(chunk.len());
        let count = stream.read(&mut chunk[..read_limit])?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(())
}

fn read_to_eof(stream: &mut TcpStream, bytes: &mut Vec<u8>, limit: usize) -> io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk)? {
            0 => return Ok(()),
            count
                if bytes
                    .len()
                    .checked_add(count)
                    .is_some_and(|length| length <= limit) =>
            {
                bytes.extend_from_slice(&chunk[..count]);
            }
            _ => return Err(invalid_data("response exceeded size limit")),
        }
    }
}

fn read_one(stream: &mut TcpStream, bytes: &mut Vec<u8>) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte)? {
        0 => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        _ => {
            bytes.push(byte[0]);
            Ok(())
        }
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
