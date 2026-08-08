use std::sync::OnceLock;

/// Borrow these bytes as text, decoding lossily once when they are not UTF-8.
///
/// One definition for both directions: a request body and a response body are
/// the same question — bytes a peer sent that an application wants to read as a
/// string — and two spellings of it are two places the caching rule can drift.
/// Bytes that are already UTF-8 are borrowed out of their own storage and cost
/// nothing; the cache holds only the replacement-decoded copy, so it is filled
/// exactly once and only for a body that needed it.
pub(super) fn lossy_text<'a>(bytes: &'a [u8], cache: &'a OnceLock<Box<str>>) -> &'a str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => cache.get_or_init(|| String::from_utf8_lossy(bytes).into()),
    }
}

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
