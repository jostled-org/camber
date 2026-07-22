use std::io::{self, BufRead, BufReader, Read};
use std::net::TcpStream;

const MAX_CHUNK_LINE_BYTES: usize = 1024;
const MAX_TRAILER_BYTES: usize = 16 * 1024;

pub(crate) fn read_response_head(
    reader: &mut BufReader<TcpStream>,
) -> (u16, Vec<(String, String)>) {
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .expect("valid HTTP status");
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            return (status, headers);
        }
        let (name, value) = line.trim_end().split_once(':').expect("valid HTTP header");
        headers.push((name.to_owned(), value.trim().to_owned()));
    }
}

pub(crate) fn read_chunk(
    reader: &mut BufReader<TcpStream>,
    payload_limit: usize,
) -> io::Result<Option<Box<[u8]>>> {
    let size_line = read_crlf_line(reader, MAX_CHUNK_LINE_BYTES)?;
    let size_text = std::str::from_utf8(&size_line[..size_line.len() - 2])
        .map_err(|error| invalid_data(format!("chunk size was not UTF-8: {error}")))?;
    let size_token = size_text
        .split(';')
        .next()
        .ok_or_else(|| invalid_data("chunk size was missing"))?;
    let size = usize::from_str_radix(size_token, 16)
        .map_err(|error| invalid_data(format!("invalid chunk size: {error}")))?;
    if size > payload_limit {
        return Err(invalid_data(format!(
            "chunk payload exceeded {payload_limit}-byte limit"
        )));
    }
    if size == 0 {
        read_trailers(reader)?;
        return Ok(None);
    }

    let mut payload = vec![0_u8; size];
    reader.read_exact(&mut payload)?;
    let mut terminator = [0_u8; 2];
    reader.read_exact(&mut terminator)?;
    if terminator != *b"\r\n" {
        return Err(invalid_data("chunk payload did not end with CRLF"));
    }
    Ok(Some(payload.into_boxed_slice()))
}

pub(crate) fn read_to_eof_bounded(
    reader: &mut BufReader<TcpStream>,
    limit: usize,
) -> io::Result<Box<[u8]>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk)? {
            0 => return Ok(bytes.into_boxed_slice()),
            count
                if bytes
                    .len()
                    .checked_add(count)
                    .is_some_and(|length| length <= limit) =>
            {
                bytes.extend_from_slice(&chunk[..count]);
            }
            _ => return Err(invalid_data(format!("body exceeded {limit}-byte limit"))),
        }
    }
}

fn read_trailers(reader: &mut BufReader<TcpStream>) -> io::Result<()> {
    let mut bytes_read = 0_usize;
    loop {
        let remaining = MAX_TRAILER_BYTES
            .checked_sub(bytes_read)
            .ok_or_else(|| invalid_data("chunk trailers exceeded size limit"))?;
        let line = read_crlf_line(reader, remaining)?;
        bytes_read = bytes_read
            .checked_add(line.len())
            .ok_or_else(|| invalid_data("chunk trailer length overflowed"))?;
        if line == b"\r\n" {
            return Ok(());
        }
    }
}

fn read_crlf_line(reader: &mut BufReader<TcpStream>, limit: usize) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        if line.len() == limit {
            return Err(invalid_data("chunk framing line exceeded size limit"));
        }
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
