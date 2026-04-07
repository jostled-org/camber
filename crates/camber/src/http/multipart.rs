use crate::RuntimeError;
use bytes::Bytes;

use super::strip_quotes;

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

fn bad_request(msg: &'static str) -> RuntimeError {
    RuntimeError::BadRequest(msg.into())
}

type HeaderParams<'a> = Box<[(&'a str, &'a str)]>;

fn split_param(segment: &str) -> Result<(&str, &str), RuntimeError> {
    let (key, value) = segment
        .split_once('=')
        .ok_or_else(|| bad_request("invalid multipart header parameter"))?;
    let key = key.trim();
    let value = value.trim();

    match key.is_empty() || value.is_empty() {
        true => Err(bad_request("invalid multipart header parameter")),
        false => Ok((key, strip_quotes(value))),
    }
}

fn set_str_param_once<'a>(
    slot: &mut Option<&'a str>,
    value: &'a str,
    err: &'static str,
) -> Result<(), RuntimeError> {
    match slot.is_some() || value.is_empty() {
        true => Err(bad_request(err)),
        false => {
            *slot = Some(value);
            Ok(())
        }
    }
}

fn set_boxed_param_once(
    slot: &mut Option<Box<str>>,
    value: &str,
    err: &'static str,
) -> Result<(), RuntimeError> {
    match slot.is_some() {
        true => Err(bad_request(err)),
        false => {
            *slot = Some(Box::from(value));
            Ok(())
        }
    }
}

fn split_header_segments(header: &str) -> Result<Box<[&str]>, RuntimeError> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    let mut escaped = false;

    for (idx, ch) in header.char_indices() {
        match (in_quotes, escaped, ch) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => in_quotes = false,
            (false, _, '"') => in_quotes = true,
            (false, _, ';') => {
                segments.push(header[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }

    match in_quotes {
        true => Err(bad_request("invalid multipart header parameter")),
        false => {
            segments.push(header[start..].trim());
            Ok(segments.into_boxed_slice())
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    match from >= haystack.len() || needle.is_empty() {
        true => None,
        false => haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|pos| pos + from),
    }
}

fn split_header_params(header: &str) -> Result<(&str, HeaderParams<'_>), RuntimeError> {
    let mut segments = split_header_segments(header)?.into_iter();
    let head = segments
        .next()
        .filter(|segment| !segment.is_empty())
        .ok_or_else(|| bad_request("invalid multipart header"))?;

    let mut params = Vec::new();
    for segment in segments {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            return Err(bad_request("invalid multipart header parameter"));
        }
        params.push(split_param(trimmed)?);
    }

    Ok((head, params.into_boxed_slice()))
}

fn extract_boundary(content_type: &str) -> Result<Box<str>, RuntimeError> {
    let (media_type, params) = split_header_params(content_type)?;
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err(bad_request("missing or invalid multipart boundary"));
    }

    let mut boundary: Option<&str> = None;
    for (key, value) in params {
        match key.eq_ignore_ascii_case("boundary") {
            true => set_str_param_once(
                &mut boundary,
                value,
                "missing or invalid multipart boundary",
            )?,
            false => {}
        }
    }

    boundary
        .map(Box::from)
        .ok_or_else(|| bad_request("missing or invalid multipart boundary"))
}

fn parse_content_disposition(
    header_value: &str,
) -> Result<(Box<str>, Option<Box<str>>), RuntimeError> {
    let (disposition, params) = split_header_params(header_value)?;
    if !disposition.eq_ignore_ascii_case("form-data") {
        return Err(bad_request("invalid multipart content-disposition"));
    }

    let mut name: Option<&str> = None;
    let mut filename: Option<&str> = None;

    for (key, value) in params {
        match (
            key.eq_ignore_ascii_case("name"),
            key.eq_ignore_ascii_case("filename"),
        ) {
            (true, false) => {
                set_str_param_once(&mut name, value, "invalid multipart content-disposition")?
            }
            (false, true) => set_str_param_once(
                &mut filename,
                value,
                "invalid multipart content-disposition",
            )?,
            _ => {}
        }
    }

    let name = name.ok_or_else(|| bad_request("invalid multipart content-disposition"))?;
    Ok((Box::from(name), filename.map(Box::from)))
}

fn parse_part_header(
    header_name: &str,
    header_value: &str,
    saw_disposition: &mut bool,
    name: &mut Option<Box<str>>,
    filename: &mut Option<Box<str>>,
    content_type: &mut Option<Box<str>>,
) -> Result<(), RuntimeError> {
    match (
        header_name.eq_ignore_ascii_case("content-disposition"),
        header_name.eq_ignore_ascii_case("content-type"),
        *saw_disposition,
    ) {
        (true, false, true) => Err(bad_request("invalid multipart content-disposition")),
        (true, false, false) => {
            let (parsed_name, parsed_filename) = parse_content_disposition(header_value)?;
            *name = Some(parsed_name);
            *filename = parsed_filename;
            *saw_disposition = true;
            Ok(())
        }
        (false, true, _) => {
            set_boxed_param_once(content_type, header_value, "invalid multipart part headers")
        }
        _ => Ok(()),
    }
}

fn parse_part(raw: &[u8], full_body: &Bytes, offset: usize) -> Result<Part, RuntimeError> {
    let header_end = find_bytes(raw, b"\r\n\r\n", 0)
        .ok_or_else(|| bad_request("invalid multipart part framing"))?;
    let headers_str = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| bad_request("invalid multipart part headers"))?;

    let mut name: Option<Box<str>> = None;
    let mut filename: Option<Box<str>> = None;
    let mut content_type: Option<Box<str>> = None;
    let mut saw_disposition = false;

    for line in headers_str.split("\r\n") {
        let (header_name, header_value) = line
            .split_once(':')
            .ok_or_else(|| bad_request("invalid multipart part headers"))?;
        parse_part_header(
            header_name.trim(),
            header_value.trim(),
            &mut saw_disposition,
            &mut name,
            &mut filename,
            &mut content_type,
        )?;
    }

    let data_start = header_end + 4;
    let data_offset = offset + data_start;
    let data = full_body.slice(data_offset..data_offset + (raw.len() - data_start));

    Ok(Part {
        name: name.ok_or_else(|| bad_request("invalid multipart content-disposition"))?,
        filename,
        content_type,
        data,
    })
}

enum Delimiter {
    NextPart(usize),
    End,
}

fn parse_delimiter_suffix(body: &[u8], pos: usize) -> Result<Delimiter, RuntimeError> {
    match body.get(pos..pos + 2) {
        Some(b"\r\n") => Ok(Delimiter::NextPart(pos + 2)),
        Some(b"--") => parse_closing_delimiter(body, pos + 2),
        _ => Err(bad_request("invalid multipart delimiter framing")),
    }
}

fn parse_closing_delimiter(body: &[u8], end: usize) -> Result<Delimiter, RuntimeError> {
    match (end == body.len(), body.get(end..end + 2)) {
        (true, _) => Ok(Delimiter::End),
        (false, Some(b"\r\n")) if end + 2 == body.len() => Ok(Delimiter::End),
        _ => Err(bad_request("invalid multipart delimiter framing")),
    }
}

fn find_next_delimiter(body: &[u8], marker: &[u8], from: usize) -> Option<usize> {
    let mut search_from = from;

    loop {
        let pos = find_bytes(body, marker, search_from)?;
        let suffix = pos + marker.len();

        match body.get(suffix..suffix + 2) {
            Some(b"\r\n") | Some(b"--") => return Some(pos),
            _ => search_from = pos + marker.len(),
        }
    }
}

/// Parse a multipart/form-data body into parts.
///
/// The body must already be fully buffered. The boundary is extracted from
/// the Content-Type header.
pub(crate) fn parse(content_type: &str, body: &Bytes) -> Result<MultipartReader, RuntimeError> {
    let boundary = extract_boundary(content_type)?;
    let opening = format!("--{boundary}");
    let opening_bytes = opening.as_bytes();

    if !body.starts_with(opening_bytes) {
        return Err(bad_request("invalid multipart delimiter framing"));
    }

    let mut pos = match parse_delimiter_suffix(body, opening_bytes.len())? {
        Delimiter::NextPart(next) => next,
        Delimiter::End => {
            return Ok(MultipartReader {
                parts: Box::new([]),
            });
        }
    };

    let next_marker = format!("\r\n--{boundary}");
    let next_marker_bytes = next_marker.as_bytes();
    let mut parts = Vec::new();

    loop {
        let next_delim = find_next_delimiter(body, next_marker_bytes, pos)
            .ok_or_else(|| bad_request("invalid multipart delimiter framing"))?;
        let raw_part = &body[pos..next_delim];
        parts.push(parse_part(raw_part, body, pos)?);

        let suffix_pos = next_delim + next_marker_bytes.len();
        match parse_delimiter_suffix(body, suffix_pos)? {
            Delimiter::NextPart(next) => pos = next,
            Delimiter::End => break,
        }
    }

    Ok(MultipartReader {
        parts: parts.into_boxed_slice(),
    })
}
