use crate::ws_frame_io::{read_ws_frame_raw, write_masked_frame};
use std::io::Write;
use std::net::TcpStream;

pub fn read_ws_text_frame(stream: &mut TcpStream) -> String {
    let (_, payload) = read_ws_frame_raw(stream);
    String::from_utf8(payload).unwrap()
}

pub fn write_ws_text_frame(stream: &mut TcpStream, text: &str) {
    write_masked_frame(stream, 0x01, text.as_bytes());
}

pub fn write_ws_close_frame(stream: &mut TcpStream) {
    let frame = [0x88, 0x80, 0x00, 0x00, 0x00, 0x00];
    stream.write_all(&frame).unwrap();
}
