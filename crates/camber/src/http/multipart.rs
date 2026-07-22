use crate::RuntimeError;
use bytes::Bytes;
use std::borrow::Cow;

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

#[derive(Clone, Copy)]
enum ParameterValue<'a> {
    Quoted(&'a str),
    Unquoted(&'a str),
}

impl<'a> ParameterValue<'a> {
    fn decode(self) -> Cow<'a, str> {
        match self {
            Self::Unquoted(value) => Cow::Borrowed(value),
            Self::Quoted(value) => decode_quoted_value(value),
        }
    }
}

struct HeaderParameter<'a> {
    name: &'a str,
    value: ParameterValue<'a>,
}

struct HeaderParameters<'a> {
    remaining: Option<&'a str>,
}

impl<'a> Iterator for HeaderParameters<'a> {
    type Item = Result<HeaderParameter<'a>, RuntimeError>;

    fn next(&mut self) -> Option<Self::Item> {
        let remaining = self.remaining.take()?;
        Some(match split_next_parameter(remaining) {
            Ok((parameter, next)) => {
                self.remaining = next;
                parse_parameter(parameter)
            }
            Err(error) => Err(error),
        })
    }
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_quoted_text(ch: char) -> bool {
    match ch {
        '\t' | ' ' | '!' | '#'..='[' | ']'..='~' => true,
        _ => !ch.is_ascii(),
    }
}

fn is_quoted_pair_value(ch: char) -> bool {
    match ch {
        '\t' | ' '..='~' => true,
        _ => !ch.is_ascii(),
    }
}

fn validate_quoted_value(value: &str) -> Result<(), RuntimeError> {
    let mut chars = value.chars();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => validate_quoted_pair(chars.next())?,
            _ if is_quoted_text(ch) => {}
            _ => return Err(bad_request("invalid multipart quoted parameter")),
        }
    }

    Ok(())
}

fn validate_quoted_pair(value: Option<char>) -> Result<(), RuntimeError> {
    match value {
        Some(escaped) if is_quoted_pair_value(escaped) => Ok(()),
        _ => Err(bad_request("invalid multipart quoted parameter")),
    }
}

fn decode_quoted_value(value: &str) -> Cow<'_, str> {
    if !value.contains('\\') {
        return Cow::Borrowed(value);
    }

    let mut decoded = String::with_capacity(value.len());
    let mut escaped = false;
    for ch in value.chars() {
        match (escaped, ch) {
            (true, _) => {
                decoded.push(ch);
                escaped = false;
            }
            (false, '\\') => escaped = true,
            (false, _) => decoded.push(ch),
        }
    }
    Cow::Owned(decoded)
}

fn parse_parameter_value(value: &str) -> Result<ParameterValue<'_>, RuntimeError> {
    if let Some(quoted) = value.strip_prefix('"') {
        let inner = quoted
            .strip_suffix('"')
            .ok_or_else(|| bad_request("invalid multipart quoted parameter"))?;
        validate_quoted_value(inner)?;
        return Ok(ParameterValue::Quoted(inner));
    }

    match !value.is_empty() && value.bytes().all(is_token_byte) {
        true => Ok(ParameterValue::Unquoted(value)),
        false => Err(bad_request("invalid multipart unquoted parameter")),
    }
}

fn parse_parameter(segment: &str) -> Result<HeaderParameter<'_>, RuntimeError> {
    let (key, value) = segment
        .split_once('=')
        .ok_or_else(|| bad_request("invalid multipart header parameter"))?;
    let key = key.trim();
    let value = value.trim();

    match !key.is_empty() && key.bytes().all(is_token_byte) {
        true => Ok(HeaderParameter {
            name: key,
            value: parse_parameter_value(value)?,
        }),
        false => Err(bad_request("invalid multipart parameter name")),
    }
}

fn set_owned_param_once(
    slot: &mut Option<Box<str>>,
    value: ParameterValue<'_>,
    err: &'static str,
) -> Result<(), RuntimeError> {
    if slot.is_some() {
        return Err(bad_request(err));
    }

    let decoded = value.decode();
    if decoded.is_empty() {
        return Err(bad_request(err));
    }
    *slot = Some(match decoded {
        Cow::Borrowed(value) => Box::from(value),
        Cow::Owned(value) => value.into_boxed_str(),
    });
    Ok(())
}

fn split_next_parameter(input: &str) -> Result<(&str, Option<&str>), RuntimeError> {
    let mut in_quotes = false;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        match (in_quotes, escaped, ch) {
            (true, true, _) => escaped = false,
            (true, false, '\\') => escaped = true,
            (true, false, '"') => in_quotes = false,
            (false, _, '"') => in_quotes = true,
            (false, _, ';') => {
                let segment = input[..index].trim();
                let remaining = &input[index + 1..];
                return parameter_segment(segment, remaining);
            }
            _ => {}
        }
    }

    if in_quotes || escaped {
        return Err(bad_request("invalid multipart quoted parameter"));
    }

    let segment = input.trim();
    match segment.is_empty() {
        true => Err(bad_request("invalid multipart header parameter")),
        false => Ok((segment, None)),
    }
}

fn parameter_segment<'a>(
    segment: &'a str,
    remaining: &'a str,
) -> Result<(&'a str, Option<&'a str>), RuntimeError> {
    match segment.is_empty() {
        true => Err(bad_request("invalid multipart header parameter")),
        false => Ok((segment, Some(remaining))),
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

fn split_header_params(header: &str) -> Result<(&str, HeaderParameters<'_>), RuntimeError> {
    let (head, remaining) = match header.split_once(';') {
        Some((head, parameters)) => (head.trim(), Some(parameters)),
        None => (header.trim(), None),
    };
    match head.is_empty() {
        true => Err(bad_request("invalid multipart header")),
        false => Ok((head, HeaderParameters { remaining })),
    }
}

fn is_boundary_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'\''
                | b'('
                | b')'
                | b'+'
                | b'_'
                | b','
                | b'-'
                | b'.'
                | b'/'
                | b':'
                | b'='
                | b'?'
                | b' '
        )
}

fn validate_boundary(boundary: &str) -> Result<(), RuntimeError> {
    let valid_length = matches!(boundary.len(), 1..=70);
    let valid_characters = boundary.bytes().all(is_boundary_char);
    let valid_ending = !boundary.ends_with(' ');
    match valid_length && valid_characters && valid_ending {
        true => Ok(()),
        false => Err(bad_request("missing or invalid multipart boundary")),
    }
}

fn extract_boundary(content_type: &str) -> Result<Cow<'_, str>, RuntimeError> {
    let (media_type, params) = split_header_params(content_type)?;
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err(bad_request("missing or invalid multipart boundary"));
    }

    let mut boundary = None;
    for parameter in params {
        let parameter = parameter?;
        match parameter.name.eq_ignore_ascii_case("boundary") {
            true if boundary.is_some() => {
                return Err(bad_request("missing or invalid multipart boundary"));
            }
            true => {
                let decoded = parameter.value.decode();
                validate_boundary(&decoded)?;
                boundary = Some(decoded);
            }
            false => {}
        }
    }

    boundary.ok_or_else(|| bad_request("missing or invalid multipart boundary"))
}

fn parse_content_disposition(
    header_value: &str,
) -> Result<(Box<str>, Option<Box<str>>), RuntimeError> {
    let (disposition, params) = split_header_params(header_value)?;
    if !disposition.eq_ignore_ascii_case("form-data") {
        return Err(bad_request("invalid multipart content-disposition"));
    }

    let mut name = None;
    let mut filename = None;

    for parameter in params {
        let parameter = parameter?;
        match (
            parameter.name.eq_ignore_ascii_case("name"),
            parameter.name.eq_ignore_ascii_case("filename"),
        ) {
            (true, false) => set_owned_param_once(
                &mut name,
                parameter.value,
                "invalid multipart content-disposition",
            )?,
            (false, true) => set_owned_param_once(
                &mut filename,
                parameter.value,
                "invalid multipart content-disposition",
            )?,
            _ => {}
        }
    }

    let name = name.ok_or_else(|| bad_request("invalid multipart content-disposition"))?;
    Ok((name, filename))
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
        content_type.is_some(),
    ) {
        (true, false, true, _) => Err(bad_request("invalid multipart content-disposition")),
        (true, false, false, _) => {
            let (parsed_name, parsed_filename) = parse_content_disposition(header_value)?;
            *name = Some(parsed_name);
            *filename = parsed_filename;
            *saw_disposition = true;
            Ok(())
        }
        (false, true, _, true) => Err(bad_request("invalid multipart part headers")),
        (false, true, _, false) => {
            *content_type = Some(Box::from(header_value));
            Ok(())
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

fn find_next_delimiter(body: &[u8], boundary: &[u8], from: usize) -> Option<usize> {
    const DELIMITER_PREFIX: &[u8] = b"\r\n--";
    let mut search_from = from;

    loop {
        let pos = find_bytes(body, DELIMITER_PREFIX, search_from)?;
        let boundary_start = pos + DELIMITER_PREFIX.len();
        let suffix = boundary_start + boundary.len();
        let boundary_matches = body
            .get(boundary_start..suffix)
            .is_some_and(|candidate| candidate == boundary);

        match (boundary_matches, body.get(suffix..suffix + 2)) {
            (true, Some(b"\r\n") | Some(b"--")) => return Some(pos),
            _ => search_from = boundary_start,
        }
    }
}

/// Parse a multipart/form-data body into parts.
///
/// The body must already be fully buffered. The boundary is extracted from
/// the Content-Type header.
pub(crate) fn parse(content_type: &str, body: &Bytes) -> Result<MultipartReader, RuntimeError> {
    let boundary = extract_boundary(content_type)?;
    let boundary_bytes = boundary.as_bytes();
    let opening_length = boundary_bytes.len() + 2;

    if !body.starts_with(b"--") || !body[2..].starts_with(boundary_bytes) {
        return Err(bad_request("invalid multipart delimiter framing"));
    }

    let mut pos = match parse_delimiter_suffix(body, opening_length)? {
        Delimiter::NextPart(next) => next,
        Delimiter::End => {
            return Ok(MultipartReader {
                parts: Box::new([]),
            });
        }
    };

    let mut parts = Vec::new();

    loop {
        let next_delim = find_next_delimiter(body, boundary_bytes, pos)
            .ok_or_else(|| bad_request("invalid multipart delimiter framing"))?;
        let raw_part = &body[pos..next_delim];
        parts.push(parse_part(raw_part, body, pos)?);

        let suffix_pos = next_delim + 4 + boundary_bytes.len();
        match parse_delimiter_suffix(body, suffix_pos)? {
            Delimiter::NextPart(next) => pos = next,
            Delimiter::End => break,
        }
    }

    Ok(MultipartReader {
        parts: parts.into_boxed_slice(),
    })
}
