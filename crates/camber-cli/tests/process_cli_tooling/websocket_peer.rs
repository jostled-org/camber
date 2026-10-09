//! A minimal blocking WebSocket client for one exchange with a generated app.
//!
//! It speaks just enough RFC 6455 to prove a live route: the opening handshake,
//! one masked text frame, the echoed frame, and the closing handshake. Every
//! read and write is bounded by the stream's timeouts.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use crate::support::FixtureError;
use crate::support::http_head::{header_value, status_code};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEAD: usize = 16 * 1024;
const MAX_PAYLOAD: usize = 64 * 1024;
/// The sample key from RFC 6455 section 1.3, and the accept value it fixes.
const HANDSHAKE_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const HANDSHAKE_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_CLOSE: u8 = 0x8;
const NORMAL_CLOSURE: [u8; 2] = [0x03, 0xe8];

/// One received frame: its opcode and its unmasked payload.
struct Frame {
    opcode: u8,
    payload: Box<[u8]>,
}

/// Upgrade `path` on `addr`, send `text`, and return the text frame that
/// answers it. The client starts the closing handshake before it returns.
pub fn exchange_text(addr: SocketAddr, path: &str, text: &str) -> Result<Box<str>, FixtureError> {
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    upgrade(&mut stream, addr, path)?;
    write_frame(&mut stream, OPCODE_TEXT, text.as_bytes())?;
    let answer = read_frame(&mut stream)?;
    let reply = match answer.opcode {
        OPCODE_TEXT => String::from_utf8(answer.payload.into_vec())?.into_boxed_str(),
        opcode => {
            return Err(FixtureError::new(format!(
                "expected a text frame, got opcode {opcode:#x}"
            )));
        }
    };
    close(&mut stream)?;
    Ok(reply)
}

fn upgrade(stream: &mut TcpStream, addr: SocketAddr, path: &str) -> Result<(), FixtureError> {
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {HANDSHAKE_KEY}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes())?;
    let head = read_head(stream)?;
    let switched = status_code(&head) == Some(101);
    let accepted = header_value(&head, "sec-websocket-accept") == Some(HANDSHAKE_ACCEPT);
    match switched && accepted {
        true => Ok(()),
        false => Err(FixtureError::new(format!(
            "upgrade was not accepted: {head}"
        ))),
    }
}

/// Read the response head one byte at a time, so no frame byte after it is
/// consumed with it.
fn read_head(stream: &mut TcpStream) -> Result<Box<str>, FixtureError> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_HEAD {
            return Err(FixtureError::new(
                "upgrade response head exceeded its limit",
            ));
        }
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    Ok(String::from_utf8(head)?.into_boxed_str())
}

/// Write one final client frame. A client must mask every frame it sends.
fn write_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> Result<(), FixtureError> {
    let length = u8::try_from(payload.len())
        .ok()
        .filter(|length| *length < 126)
        .ok_or_else(|| FixtureError::new("test frame payload must stay below 126 bytes"))?;
    let mut frame = vec![0x80 | opcode, 0x80 | length];
    frame.extend_from_slice(&MASK);
    frame.extend(
        payload
            .iter()
            .zip(MASK.iter().cycle())
            .map(|(byte, mask)| byte ^ mask),
    );
    stream.write_all(&frame)?;
    Ok(())
}

/// Read one unmasked server frame with a 7-bit or 16-bit length.
fn read_frame(stream: &mut TcpStream) -> Result<Frame, FixtureError> {
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header)?;
    let length = match header[1] {
        length if length & 0x80 != 0 => {
            return Err(FixtureError::new("server frame was masked"));
        }
        126 => {
            let mut extended = [0_u8; 2];
            stream.read_exact(&mut extended)?;
            usize::from(u16::from_be_bytes(extended))
        }
        127 => return Err(FixtureError::new("server frame used a 64-bit length")),
        length => usize::from(length),
    };
    if length > MAX_PAYLOAD {
        return Err(FixtureError::new("server frame exceeded its limit"));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok(Frame {
        opcode: header[0] & 0x0f,
        payload: payload.into_boxed_slice(),
    })
}

/// Send a normal close and read the server's close answer.
fn close(stream: &mut TcpStream) -> Result<(), FixtureError> {
    write_frame(stream, OPCODE_CLOSE, &NORMAL_CLOSURE)?;
    let answer = read_frame(stream)?;
    match answer.opcode {
        OPCODE_CLOSE => Ok(()),
        opcode => Err(FixtureError::new(format!(
            "expected a close frame, got opcode {opcode:#x}"
        ))),
    }
}
