use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const WS_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// The longest a whole frame can be: the payload limit plus the widest header —
/// two opcode-and-length bytes, an eight-byte extended length, and a four-byte
/// mask key.
const MAX_FRAME_CAPTURE: usize = MAX_FRAME_BYTES + 14;

/// A handshake key Camber accepts. The accept value derived from it is the
/// WebSocket suite's own subject; every other journey only needs the `101`.
pub const WS_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// The upgrade request every handshake journey sends.
///
/// One definition of the handshake for the whole workspace: a copy per harness
/// is a copy that can drift from what Camber actually accepts.
pub fn ws_upgrade_request(path: &str) -> Box<str> {
    ws_upgrade_request_to("localhost", path)
}

/// The same upgrade request, addressed to one named authority.
///
/// The authority is a parameter rather than a spliced extra header: a
/// handshake carrying two `Host` values is a different request than one
/// addressed to a host router, and a case about host routing would be proving
/// something about neither.
pub fn ws_upgrade_request_to(host: &str, path: &str) -> Box<str> {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: {WS_KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
    .into_boxed_str()
}

/// The upgrade request carrying `extra` headers, for a case whose claim is what
/// one added header does.
///
/// Built on [`ws_upgrade_request`] rather than beside it: a harness that spelled
/// the handshake out again to add a header would be proving something about a
/// request no client sends. Only the added headers are spliced in, ahead of the
/// blank line that ends the head.
pub fn ws_upgrade_request_with(path: &str, extra: &[(&str, &str)]) -> Box<str> {
    let head = ws_upgrade_request(path);
    let mut request = head
        .strip_suffix("\r\n")
        .expect("the shared upgrade request no longer ends with its blank line")
        .to_owned();
    super::http::append_headers(&mut request, extra.iter().copied());
    request.push_str("\r\n");
    request.into_boxed_str()
}

/// Open a connection and send the upgrade request without reading anything.
///
/// The response is left on the socket so a caller can order its own
/// observations against the handshake rather than against a helper's read.
pub fn start_upgrade(addr: SocketAddr, path: &str) -> TcpStream {
    start_upgrade_with(addr, &ws_upgrade_request(path))
}

/// [`start_upgrade`] for a caller that built its own head.
///
/// The head is the parameter because the head is what those callers vary: a
/// handshake missing a required header, one declaring a version Camber does not
/// speak, one addressed to a named authority. Three of them wrote the connect,
/// the write, and the flush out for themselves, so a peer that could not connect
/// and a head that could not be sent were told apart three times over.
///
/// The answer is left on the socket, for the reason [`start_upgrade`] gives.
pub fn start_upgrade_with(addr: SocketAddr, head: &str) -> TcpStream {
    let mut peer = super::http::connect(addr).expect("failed to connect the WebSocket peer");
    peer.write_all(head.as_bytes())
        .expect("failed to send the WebSocket handshake");
    peer.flush()
        .expect("failed to flush the WebSocket handshake");
    peer
}

/// Read the handshake response head: every byte through the blank line that
/// ends it.
///
/// Sealed, because nothing appends to a head once it is framed. Framed by the
/// HTTP reader rather than by a second scan loop here: that reader bounds the
/// whole head against one deadline, where a socket timeout applies per read and
/// a peer trickling one byte at a time would hold the handshake open for as
/// many timeouts as it sends bytes.
pub fn read_until_double_crlf(stream: &mut TcpStream) -> Box<str> {
    try_read_until_double_crlf(stream).expect("failed to read the WebSocket handshake response")
}

/// Read one binary frame's payload, sealed.
///
/// `read_ws_frame_raw` already hands back a `Box<[u8]>` and no caller appends
/// to the result, so re-opening it into a `Vec` would buy a spare capacity
/// field nothing uses.
pub fn read_ws_binary_frame(stream: &mut TcpStream) -> Box<[u8]> {
    read_ws_frame_raw(stream).1
}

pub fn write_ws_binary_frame(stream: &mut TcpStream, data: &[u8]) {
    write_masked_frame(stream, 0x02, data);
}

/// Read one text frame's payload, sealed.
///
/// Nothing downstream appends to a received message, so the text is handed back
/// as [`read_ws_frame_raw`] hands back its bytes: `into_boxed_str` shrinks the
/// buffer in place rather than paying for a spare capacity field no caller uses.
pub fn read_ws_text_frame(stream: &mut TcpStream) -> Box<str> {
    String::from_utf8(read_ws_frame_raw(stream).1.into_vec())
        .expect("the WebSocket text frame carried a payload that was not UTF-8")
        .into_boxed_str()
}

pub fn write_ws_text_frame(stream: &mut TcpStream, text: &str) {
    write_masked_frame(stream, 0x01, text.as_bytes());
}

/// Send the masked, empty close frame.
///
/// A peer that has already gone is the expected outcome and is accepted. Every
/// other failure is not: a close frame that timed out because the send buffer
/// filled, or that was refused outright, never reached the server — and the
/// disconnect probes then read a cause on the assumption it did.
pub fn write_ws_close_frame(stream: &mut TcpStream) {
    match with_write_timeout(stream, |stream| {
        stream.write_all(&[0x88, 0x80, 0x00, 0x00, 0x00, 0x00])
    }) {
        Ok(()) => {}
        Err(error) if super::http::is_closed_connection_error(&error) => {}
        Err(error) => panic!("the WebSocket close frame could not be sent: {error}"),
    }
}

/// One frame's opcode and its unmasked payload.
///
/// The payload is built mutably — unmasking rewrites every byte — and handed
/// back sealed, because nothing downstream of the unmask pass writes to it.
pub fn read_ws_frame_raw(stream: &mut TcpStream) -> (u8, Box<[u8]>) {
    try_read_ws_frame_raw(stream).expect("failed to read a WebSocket frame")
}

pub fn write_masked_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    try_write_masked_frame(stream, opcode, payload)
        .expect("failed to write a masked WebSocket frame");
}

fn try_read_until_double_crlf(stream: &mut TcpStream) -> io::Result<Box<str>> {
    let head = super::http::read_head(stream, WS_IO_TIMEOUT)?;
    Ok(String::from_utf8_lossy(&head).into_owned().into_boxed_str())
}

/// Read one frame against a single deadline over the whole of it.
///
/// A frame arrives in up to four reads — the two header bytes, an extended
/// length, the mask key, the payload — and a socket timeout bounds one syscall
/// each. Under that, a peer dribbling one byte per read holds the frame open for
/// as many timeouts as it sends bytes. One deadline computed here and narrowed
/// before every read is what bounds the frame as a whole.
///
/// Public alongside [`read_ws_frame_raw`], for the callers whose claim is the
/// failure rather than the frame. A transport that was given up reads as
/// `UnexpectedEof` or `ConnectionReset`, and one still open that answered
/// nothing reads as `TimedOut`; the panic the sealed reader raises carries the
/// two back as the same caught unwind.
pub fn try_read_ws_frame_raw(stream: &mut TcpStream) -> io::Result<(u8, Box<[u8]>)> {
    let deadline = Instant::now() + WS_IO_TIMEOUT;
    super::http::with_socket_timeout(
        stream,
        super::http::READ_TIMEOUT,
        Some(WS_IO_TIMEOUT),
        |stream| {
            // One buffer for the whole frame: each leg fills to an absolute
            // length and reports where its own bytes begin, so the header, the
            // mask key, and the payload are read without a buffer each.
            let mut frame = Vec::new();
            let header: [u8; 2] = fill(stream, &mut frame, 2, deadline)?
                .try_into()
                .expect("the header fill left exactly two bytes");
            let opcode = header[0] & 0x0f;
            let masked = (header[1] & 0x80) != 0;
            let length = read_frame_length(stream, &mut frame, header[1] & 0x7f, deadline)?;
            if length > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WebSocket frame exceeded size limit",
                ));
            }
            let mask: Option<[u8; 4]> = match masked {
                true => Some(
                    fill(stream, &mut frame, 4, deadline)?
                        .try_into()
                        .expect("the mask fill left exactly four bytes"),
                ),
                false => None,
            };
            let mut payload = fill(stream, &mut frame, length, deadline)?.to_vec();
            if let Some(key) = mask {
                payload
                    .iter_mut()
                    .enumerate()
                    .for_each(|(index, byte)| *byte ^= key[index % 4]);
            }
            Ok((opcode, payload.into_boxed_slice()))
        },
    )
}

/// Grow `frame` by `count` bytes against `deadline`, and hand back just those
/// bytes.
fn fill<'a>(
    stream: &mut TcpStream,
    frame: &'a mut Vec<u8>,
    count: usize,
    deadline: Instant,
) -> io::Result<&'a [u8]> {
    let start = frame.len();
    let end = start.checked_add(count).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "WebSocket frame length overflowed",
        )
    })?;
    super::http::read_to_length(
        stream,
        frame,
        end,
        MAX_FRAME_CAPTURE,
        "WebSocket frame",
        Some(deadline),
    )?;
    Ok(&frame[start..])
}

fn read_frame_length(
    stream: &mut TcpStream,
    frame: &mut Vec<u8>,
    short: u8,
    deadline: Instant,
) -> io::Result<usize> {
    match short {
        126 => {
            let bytes: [u8; 2] = fill(stream, frame, 2, deadline)?
                .try_into()
                .expect("the length fill left exactly two bytes");
            Ok(usize::from(u16::from_be_bytes(bytes)))
        }
        127 => {
            let bytes: [u8; 8] = fill(stream, frame, 8, deadline)?
                .try_into()
                .expect("the length fill left exactly eight bytes");
            usize::try_from(u64::from_be_bytes(bytes)).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WebSocket frame length overflowed",
                )
            })
        }
        length => Ok(usize::from(length)),
    }
}

fn try_write_masked_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WebSocket frame exceeded size limit",
        ));
    }
    let capacity = payload
        .len()
        .checked_add(14)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame length overflowed"))?;
    let mut frame = Vec::with_capacity(capacity);
    frame.push(0x80 | opcode);
    match payload.len() {
        length @ 0..=125 => frame.push(0x80 | length as u8),
        length @ 126..=65535 => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(&[0_u8; 4]);
    frame.extend_from_slice(payload);
    with_write_timeout(stream, |stream| stream.write_all(&frame))
}

/// Run one write under the WebSocket send bound.
///
/// A write is one syscall, so the socket's own timeout bounds the whole of it
/// and no deadline is needed.
fn with_write_timeout<T>(
    stream: &mut TcpStream,
    operation: impl FnOnce(&mut TcpStream) -> io::Result<T>,
) -> io::Result<T> {
    super::http::with_socket_timeout(
        stream,
        super::http::WRITE_TIMEOUT,
        Some(WS_IO_TIMEOUT),
        operation,
    )
}
