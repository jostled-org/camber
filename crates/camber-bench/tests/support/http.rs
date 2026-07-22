use super::FixtureError;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Box<[(Box<str>, Box<str>)]>,
    pub body: Box<[u8]>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
    }
}

pub fn get(addr: SocketAddr, path: &str, timeout: Duration) -> Result<Response, FixtureError> {
    get_with_headers(addr, path, &[], timeout)
}

pub fn get_with_headers(
    addr: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<Response, FixtureError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| FixtureError::new("HTTP deadline overflow"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|error| FixtureError::new(format!("failed to connect to {addr}: {error}")))?;
    set_stream_timeout(&stream, deadline)?;
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    read_response(&mut stream, deadline)
}

fn read_response(stream: &mut TcpStream, deadline: Instant) -> Result<Response, FixtureError> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        set_stream_timeout(stream, deadline)?;
        let remaining = MAX_RESPONSE_BYTES.saturating_sub(bytes.len());
        let probe_limit = checked_add(remaining, 1, "response read limit")?.min(buffer.len());
        let read_result = stream.read(&mut buffer[..probe_limit]);
        match handle_response_read(read_result, &buffer, &mut bytes)? {
            Some(response) => return Ok(response),
            None => {}
        }
    }
}

fn handle_response_read(
    read_result: std::io::Result<usize>,
    buffer: &[u8],
    bytes: &mut Vec<u8>,
) -> Result<Option<Response>, FixtureError> {
    match read_result {
        Ok(0) => parse_response(bytes, true).map(Some),
        Ok(read) => append_response_bytes(bytes, &buffer[..read]),
        Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
            Err(FixtureError::new("response read timed out"))
        }
        Err(error) => Err(FixtureError::from(error)),
    }
}

fn append_response_bytes(
    bytes: &mut Vec<u8>,
    chunk: &[u8],
) -> Result<Option<Response>, FixtureError> {
    let new_len = checked_add(bytes.len(), chunk.len(), "response length")?;
    match new_len <= MAX_RESPONSE_BYTES {
        true => bytes.extend_from_slice(chunk),
        false => return Err(FixtureError::new("HTTP response exceeded 1 MiB limit")),
    }
    match response_is_complete(bytes)? {
        true => parse_response(bytes, false).map(Some),
        false => Ok(None),
    }
}

fn response_is_complete(bytes: &[u8]) -> Result<bool, FixtureError> {
    let Some(header_end) = find_bytes(bytes, b"\r\n\r\n") else {
        return Ok(false);
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| FixtureError::new(format!("invalid HTTP headers: {error}")))?;
    let content_length = headers.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>())
    });
    let chunked = headers.lines().skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        })
    });
    let body_start = checked_add(header_end, 4, "HTTP body offset")?;
    if chunked {
        let body = bytes
            .get(body_start..)
            .ok_or_else(|| FixtureError::new("HTTP body offset exceeds response"))?;
        return decode_chunked(body).map(|body| body.is_some());
    }
    match content_length {
        Some(Ok(length)) => content_length_is_complete(bytes.len(), body_start, length),
        Some(Err(error)) => Err(FixtureError::new(format!(
            "invalid HTTP content length: {error}"
        ))),
        None => Ok(false),
    }
}

fn content_length_is_complete(
    received: usize,
    body_start: usize,
    length: usize,
) -> Result<bool, FixtureError> {
    let frame_end = checked_add(body_start, length, "HTTP body length")?;
    match frame_end <= MAX_RESPONSE_BYTES {
        true => Ok(received >= frame_end),
        false => Err(FixtureError::new("HTTP response exceeded 1 MiB limit")),
    }
}

fn parse_response(bytes: &[u8], connection_closed: bool) -> Result<Response, FixtureError> {
    let header_end = find_bytes(bytes, b"\r\n\r\n")
        .ok_or_else(|| FixtureError::new("incomplete HTTP response headers"))?;
    let header_text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| FixtureError::new(format!("invalid HTTP headers: {error}")))?;
    let mut lines = header_text.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| FixtureError::new("missing HTTP status"))?
        .parse::<u16>()
        .map_err(|error| FixtureError::new(format!("invalid HTTP status: {error}")))?;
    let headers: Box<[(Box<str>, Box<str>)]> = lines
        .map(|line| {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| FixtureError::new(format!("invalid HTTP header: {line}")))?;
            Ok((name.into(), value.trim().into()))
        })
        .collect::<Result<Vec<_>, FixtureError>>()?
        .into_boxed_slice();
    let body_start = checked_add(header_end, 4, "HTTP body offset")?;
    let body = bytes
        .get(body_start..)
        .ok_or_else(|| FixtureError::new("HTTP body offset exceeds response"))?;
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
    });
    if chunked {
        let body = decode_chunked(body)?
            .ok_or_else(|| FixtureError::new("incomplete chunked HTTP response body"))?;
        return Ok(Response {
            status,
            headers,
            body,
        });
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .transpose()
        .map_err(|error| FixtureError::new(format!("invalid HTTP content length: {error}")))?;
    match (content_length, connection_closed) {
        (Some(length), _) => parse_content_length_body(bytes, body_start, length, status, headers),
        (None, true) => Ok(Response {
            status,
            headers,
            body: body.into(),
        }),
        (None, false) => Err(FixtureError::new(
            "HTTP response has no completion boundary",
        )),
    }
}

fn parse_content_length_body(
    bytes: &[u8],
    body_start: usize,
    length: usize,
    status: u16,
    headers: Box<[(Box<str>, Box<str>)]>,
) -> Result<Response, FixtureError> {
    let frame_end = checked_add(body_start, length, "HTTP body length")?;
    if frame_end > MAX_RESPONSE_BYTES {
        return Err(FixtureError::new("HTTP response exceeded 1 MiB limit"));
    }
    bytes
        .get(body_start..frame_end)
        .map(|body| Response {
            status,
            headers,
            body: body.into(),
        })
        .ok_or_else(|| FixtureError::new("incomplete HTTP response body"))
}

fn set_stream_timeout(stream: &TcpStream, deadline: Instant) -> Result<(), FixtureError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| FixtureError::new("response read timed out"))?;
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))?;
    Ok(())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_chunked(bytes: &[u8]) -> Result<Option<Box<[u8]>>, FixtureError> {
    let mut cursor = 0;
    let mut decoded = Vec::new();
    loop {
        let remaining = bytes
            .get(cursor..)
            .ok_or_else(|| FixtureError::new("chunk cursor exceeds response"))?;
        let Some(relative_line_end) = find_bytes(remaining, b"\r\n") else {
            return Ok(None);
        };
        let line_end = checked_add(cursor, relative_line_end, "chunk line offset")?;
        let size_bytes = bytes
            .get(cursor..line_end)
            .ok_or_else(|| FixtureError::new("chunk line exceeds response"))?;
        let size_text = std::str::from_utf8(size_bytes)
            .map_err(|error| FixtureError::new(format!("invalid chunk size: {error}")))?;
        let size_text = size_text.split(';').next().unwrap_or(size_text).trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|error| FixtureError::new(format!("invalid chunk size: {error}")))?;
        cursor = checked_add(line_end, 2, "chunk data offset")?;
        if size == 0 {
            return decode_chunk_trailer(bytes, cursor, decoded);
        }
        let chunk_end = checked_add(cursor, size, "chunk size")?;
        let chunk_terminator_end = checked_add(chunk_end, 2, "chunk terminator offset")?;
        if bytes.len() < chunk_terminator_end {
            return Ok(None);
        }
        if bytes.get(chunk_end..chunk_terminator_end) != Some(b"\r\n") {
            return Err(FixtureError::new("chunk missing terminating CRLF"));
        }
        let decoded_len = checked_add(decoded.len(), size, "decoded chunk length")?;
        if decoded_len > MAX_RESPONSE_BYTES {
            return Err(FixtureError::new("HTTP response exceeded 1 MiB limit"));
        }
        let chunk = bytes
            .get(cursor..chunk_end)
            .ok_or_else(|| FixtureError::new("chunk body exceeds response"))?;
        decoded.extend_from_slice(chunk);
        cursor = chunk_terminator_end;
    }
}

fn decode_chunk_trailer(
    bytes: &[u8],
    cursor: usize,
    decoded: Vec<u8>,
) -> Result<Option<Box<[u8]>>, FixtureError> {
    let trailer_start = checked_add(cursor, 2, "chunk trailer offset")?;
    match bytes.get(cursor..trailer_start) {
        Some(b"\r\n") => Ok(Some(decoded.into_boxed_slice())),
        _ => {
            let trailers = bytes
                .get(cursor..)
                .ok_or_else(|| FixtureError::new("chunk trailer offset exceeds response"))?;
            Ok(find_bytes(trailers, b"\r\n\r\n").map(|_| decoded.into_boxed_slice()))
        }
    }
}

fn checked_add(left: usize, right: usize, context: &str) -> Result<usize, FixtureError> {
    left.checked_add(right)
        .ok_or_else(|| FixtureError::new(format!("{context} overflow")))
}
