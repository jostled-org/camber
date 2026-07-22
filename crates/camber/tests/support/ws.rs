use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const WS_IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub fn read_until_double_crlf(stream: &mut TcpStream) -> String {
    try_read_until_double_crlf(stream).unwrap()
}

pub fn read_ws_binary_frame(stream: &mut TcpStream) -> Vec<u8> {
    read_ws_frame_raw(stream).1
}

pub fn write_ws_binary_frame(stream: &mut TcpStream, data: &[u8]) {
    write_masked_frame(stream, 0x02, data);
}

pub fn read_ws_text_frame(stream: &mut TcpStream) -> String {
    String::from_utf8(read_ws_frame_raw(stream).1).unwrap()
}

pub fn write_ws_text_frame(stream: &mut TcpStream, text: &str) {
    write_masked_frame(stream, 0x01, text.as_bytes());
}

pub fn write_ws_close_frame(stream: &mut TcpStream) {
    let _ = with_write_timeout(stream, |stream| {
        stream.write_all(&[0x88, 0x80, 0x00, 0x00, 0x00, 0x00])
    });
}

pub fn read_ws_frame_raw(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    try_read_ws_frame_raw(stream).unwrap()
}

pub fn write_masked_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    try_write_masked_frame(stream, opcode, payload).unwrap();
}

fn try_read_until_double_crlf(stream: &mut TcpStream) -> io::Result<String> {
    with_read_timeout(stream, |stream| {
        let mut bytes = Vec::new();
        let mut byte = [0_u8; 1];
        while !bytes.ends_with(b"\r\n\r\n") {
            if bytes.len() >= MAX_HANDSHAKE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WebSocket handshake exceeded size limit",
                ));
            }
            stream.read_exact(&mut byte)?;
            bytes.push(byte[0]);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    })
}

fn try_read_ws_frame_raw(stream: &mut TcpStream) -> io::Result<(u8, Vec<u8>)> {
    with_read_timeout(stream, |stream| {
        let mut header = [0_u8; 2];
        stream.read_exact(&mut header)?;
        let opcode = header[0] & 0x0f;
        let masked = (header[1] & 0x80) != 0;
        let length = read_frame_length(stream, header[1] & 0x7f)?;
        if length > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WebSocket frame exceeded size limit",
            ));
        }
        let mask = match masked {
            true => {
                let mut key = [0_u8; 4];
                stream.read_exact(&mut key)?;
                Some(key)
            }
            false => None,
        };
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload)?;
        if let Some(key) = mask {
            payload
                .iter_mut()
                .enumerate()
                .for_each(|(index, byte)| *byte ^= key[index % 4]);
        }
        Ok((opcode, payload))
    })
}

fn read_frame_length(stream: &mut TcpStream, short: u8) -> io::Result<usize> {
    match short {
        126 => {
            let mut bytes = [0_u8; 2];
            stream.read_exact(&mut bytes)?;
            Ok(usize::from(u16::from_be_bytes(bytes)))
        }
        127 => {
            let mut bytes = [0_u8; 8];
            stream.read_exact(&mut bytes)?;
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

fn with_read_timeout<T>(
    stream: &mut TcpStream,
    operation: impl FnOnce(&mut TcpStream) -> io::Result<T>,
) -> io::Result<T> {
    let previous = stream.read_timeout()?;
    stream.set_read_timeout(Some(WS_IO_TIMEOUT))?;
    let result = operation(stream);
    let restore = stream.set_read_timeout(previous);
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn with_write_timeout<T>(
    stream: &mut TcpStream,
    operation: impl FnOnce(&mut TcpStream) -> io::Result<T>,
) -> io::Result<T> {
    let previous = stream.write_timeout()?;
    stream.set_write_timeout(Some(WS_IO_TIMEOUT))?;
    let result = operation(stream);
    let restore = stream.set_write_timeout(previous);
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}
