//! Materialized multipart parsing.
//!
//! The whole body is already in memory when this parser runs, so every part is
//! a slice of it and the reader hands back finished values. The grammar is not
//! this module's: it reads through [`super::grammar`], the same authority the
//! incremental parser reads through.

use super::grammar::{
    self, INVALID_CONTENT_DISPOSITION, INVALID_DELIMITER_FRAMING, PROTOCOL_MAX_BOUNDARY_BYTES,
    malformed,
};
use crate::RuntimeError;
use bytes::Bytes;

/// A parsed multipart/form-data part.
#[derive(Debug)]
pub struct Part {
    name: Box<str>,
    filename: Option<Box<str>>,
    content_type: Option<Box<str>>,
    data: Bytes,
}

impl Part {
    /// Return the multipart field name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the uploaded filename, if present.
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// Return the part content type, if present.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// Return the raw part payload.
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

/// Parsed multipart/form-data body. Provides access to all parts.
#[derive(Debug)]
pub struct MultipartReader {
    parts: Box<[Part]>,
}

impl MultipartReader {
    /// Return all parsed multipart parts.
    pub fn parts(&self) -> &[Part] {
        &self.parts
    }
}

fn parse_part(raw: &[u8], full_body: &Bytes, offset: usize) -> Result<Part, RuntimeError> {
    let header_end = grammar::find_bytes(raw, grammar::HEADER_TERMINATOR, 0)
        .ok_or_else(|| malformed("invalid multipart part framing"))?;
    let headers = grammar::parse_header_block(&raw[..header_end])?;

    let data_start = header_end + grammar::HEADER_TERMINATOR.len();
    let data_offset = offset + data_start;
    let data = full_body.slice(data_offset..data_offset + (raw.len() - data_start));

    Ok(Part {
        name: headers
            .name
            .ok_or_else(|| malformed(INVALID_CONTENT_DISPOSITION))?,
        filename: headers.filename,
        content_type: headers.content_type,
        data,
    })
}

/// Where the part after this delimiter starts, or `None` if it closed the body.
fn parse_delimiter_suffix(body: &[u8], pos: usize) -> Result<Option<usize>, RuntimeError> {
    let end = pos + grammar::DELIMITER_SUFFIX_BYTES;
    match grammar::delimiter_closes_at(body, pos)? {
        true => parse_closing_delimiter(body, end),
        false => Ok(Some(end)),
    }
}

/// Accept only the ending the grammar allows after the closing delimiter.
///
/// A materialized body has every remaining byte in hand, so an epilogue this
/// reader cannot decide yet is one this reader will never decide.
fn parse_closing_delimiter(body: &[u8], end: usize) -> Result<Option<usize>, RuntimeError> {
    let tail = body
        .get(end..)
        .ok_or_else(|| malformed(INVALID_DELIMITER_FRAMING))?;
    match grammar::epilogue_closes(tail)? {
        true => Ok(None),
        false => Err(malformed(INVALID_DELIMITER_FRAMING)),
    }
}

/// Parse a multipart/form-data body into parts.
///
/// The body must already be fully buffered. The boundary is extracted from
/// the Content-Type header.
pub(in crate::http) fn parse(
    content_type: &str,
    body: &Bytes,
) -> Result<MultipartReader, RuntimeError> {
    let boundary = grammar::extract_boundary_within(content_type, PROTOCOL_MAX_BOUNDARY_BYTES)?;
    let boundary_bytes = boundary.as_bytes();
    let opening_length = grammar::BODY_PREFIX.len() + boundary_bytes.len();

    if !grammar::opens_body(body, boundary_bytes) {
        return Err(malformed(INVALID_DELIMITER_FRAMING));
    }

    let Some(mut pos) = parse_delimiter_suffix(body, opening_length)? else {
        return Ok(MultipartReader {
            parts: Box::new([]),
        });
    };

    let mut parts = Vec::new();

    loop {
        let next_delim = grammar::find_delimiter(body, boundary_bytes, pos)
            .ok_or_else(|| malformed(INVALID_DELIMITER_FRAMING))?;
        let raw_part = &body[pos..next_delim];
        parts.push(parse_part(raw_part, body, pos)?);

        let suffix_pos = next_delim + grammar::DELIMITER_PREFIX.len() + boundary_bytes.len();
        match parse_delimiter_suffix(body, suffix_pos)? {
            Some(next) => pos = next,
            None => break,
        }
    }

    Ok(MultipartReader {
        parts: parts.into_boxed_slice(),
    })
}
