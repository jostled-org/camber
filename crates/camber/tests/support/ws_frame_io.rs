use std::io::{Read, Write};
use std::net::TcpStream;

pub fn read_until_double_crlf(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(1) => {
                buf.push(byte[0]);
                if buf.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok(_) => break,
            Err(e) => panic!("read error: {e}"),
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

pub(crate) fn read_ws_frame_raw(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).unwrap();

    let opcode = header[0] & 0x0F;
    let masked = (header[1] & 0x80) != 0;
    let mut len = (header[1] & 0x7F) as usize;

    match len {
        126 => {
            let mut ext = [0u8; 2];
            stream.read_exact(&mut ext).unwrap();
            len = u16::from_be_bytes(ext) as usize;
        }
        127 => {
            let mut ext = [0u8; 8];
            stream.read_exact(&mut ext).unwrap();
            len = u64::from_be_bytes(ext) as usize;
        }
        _ => {}
    }

    let mask_key = match masked {
        true => {
            let mut key = [0u8; 4];
            stream.read_exact(&mut key).unwrap();
            Some(key)
        }
        false => None,
    };

    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).unwrap();

    if let Some(key) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= key[i % 4];
        }
    }

    (opcode, payload)
}

pub(crate) fn write_masked_frame(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mut frame = Vec::new();
    frame.push(0x80 | opcode);

    let len = payload.len();
    match len {
        0..=125 => frame.push(0x80 | len as u8),
        126..=65535 => {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            frame.push(0x80 | 127);
            frame.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }

    let mask = [0u8; 4];
    frame.extend_from_slice(&mask);
    frame.extend_from_slice(payload);

    stream.write_all(&frame).unwrap();
}
