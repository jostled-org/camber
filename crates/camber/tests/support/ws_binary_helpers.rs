use crate::ws_frame_io::{read_ws_frame_raw, write_masked_frame};
use std::net::TcpStream;

pub fn read_ws_binary_frame(stream: &mut TcpStream) -> Vec<u8> {
    let (_, payload) = read_ws_frame_raw(stream);
    payload
}

pub fn write_ws_binary_frame(stream: &mut TcpStream, data: &[u8]) {
    write_masked_frame(stream, 0x02, data);
}
