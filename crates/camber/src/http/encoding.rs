/// Decode a percent-encoded hex pair (e.g. `4`, `1` from `%41`) into the byte value.
pub(super) fn decode_hex_pair(hi: u8, lo: u8) -> Option<u8> {
    let h = hex_digit(hi)?;
    let l = hex_digit(lo)?;
    Some(h << 4 | l)
}

/// Convert an ASCII hex digit to its numeric value (0-15).
pub(super) fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
