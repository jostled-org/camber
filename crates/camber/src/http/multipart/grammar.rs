//! The one multipart grammar both parsers read through.
//!
//! Boundary recognition, header parameters, quoted values, content-disposition,
//! and delimiter search live here and nowhere else. The buffered parser and the
//! incremental parser are two ownership models over the same wire format: a
//! second copy of these rules would let one accept a body the other refuses.

use crate::RuntimeError;
use std::borrow::Cow;

/// The multipart protocol's own maximum boundary length.
pub(super) const PROTOCOL_MAX_BOUNDARY_BYTES: usize = 70;

/// The bytes that open every delimiter line after the first.
pub(super) const DELIMITER_PREFIX: &[u8] = b"\r\n--";

/// The bytes that open the body's first delimiter line, which carries no
/// leading CRLF.
pub(super) const BODY_PREFIX: &[u8] = b"--";

/// The bytes every delimiter line ends with, saying whether it separates two
/// parts or closes the body.
pub(super) const DELIMITER_SUFFIX_BYTES: usize = 2;

/// The bytes of a delimiter line beyond the boundary itself that a streaming
/// parser must carry across a frame edge.
///
/// Recognition needs the prefix, the boundary, and the suffix. The longest
/// proper prefix of that run is one byte short of the whole, so the overhead is
/// the prefix plus the suffix, less one.
pub(super) const DELIMITER_CARRY_OVERHEAD: usize =
    DELIMITER_PREFIX.len() + DELIMITER_SUFFIX_BYTES - 1;

/// The bytes to hold while deciding an epilogue: one more than the longest one
/// the grammar accepts, so an over-long epilogue is refused, not awaited.
pub(super) const EPILOGUE_PROBE_BYTES: usize = 3;

/// The bytes that end one part's header block.
pub(super) const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// One parse refusal, named as the multipart parser's own.
///
/// The text is operator detail: it says which part of the grammar failed, and
/// the rejection boundary answers the peer with fixed safe text instead.
pub(super) fn malformed(msg: &'static str) -> RuntimeError {
    RuntimeError::Multipart(msg.into())
}

/// A quoted parameter that is unterminated, unescaped, or carries a byte the
/// grammar does not admit inside quotes.
pub(super) const INVALID_QUOTED_PARAMETER: &str = "invalid multipart quoted parameter";

/// A parameter segment with no `=`, or an empty one between two semicolons.
pub(super) const INVALID_HEADER_PARAMETER: &str = "invalid multipart header parameter";

/// A `Content-Type` that is not `multipart/form-data` with one usable boundary.
pub(super) const INVALID_BOUNDARY: &str = "missing or invalid multipart boundary";

/// A part whose `Content-Disposition` is not `form-data`, is repeated, or never
/// names the field.
pub(super) const INVALID_CONTENT_DISPOSITION: &str = "invalid multipart content-disposition";

/// A part header block that is not UTF-8, carries a line with no `:`, or
/// repeats `Content-Type`.
pub(super) const INVALID_PART_HEADERS: &str = "invalid multipart part headers";

/// A boundary line that does not open, separate, or close the body the way the
/// grammar requires.
pub(super) const INVALID_DELIMITER_FRAMING: &str = "invalid multipart delimiter framing";

#[derive(Clone, Copy)]
pub(super) enum ParameterValue<'a> {
    Quoted(&'a str),
    Unquoted(&'a str),
}

impl<'a> ParameterValue<'a> {
    pub(super) fn decode(self) -> Cow<'a, str> {
        match self {
            Self::Unquoted(value) => Cow::Borrowed(value),
            Self::Quoted(value) => decode_quoted_value(value),
        }
    }
}

pub(super) struct HeaderParameter<'a> {
    pub(super) name: &'a str,
    pub(super) value: ParameterValue<'a>,
}

pub(super) struct HeaderParameters<'a> {
    remaining: Option<&'a str>,
}

impl<'a> HeaderParameters<'a> {
    /// The next parameter this header declares, or the rule it broke.
    fn next_parameter(&mut self) -> Option<Result<HeaderParameter<'a>, RuntimeError>> {
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

impl<'a> Iterator for HeaderParameters<'a> {
    type Item = Result<HeaderParameter<'a>, RuntimeError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_parameter()
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
            _ => return Err(malformed(INVALID_QUOTED_PARAMETER)),
        }
    }

    Ok(())
}

fn validate_quoted_pair(value: Option<char>) -> Result<(), RuntimeError> {
    match value {
        Some(escaped) if is_quoted_pair_value(escaped) => Ok(()),
        _ => Err(malformed(INVALID_QUOTED_PARAMETER)),
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
            .ok_or_else(|| malformed(INVALID_QUOTED_PARAMETER))?;
        validate_quoted_value(inner)?;
        return Ok(ParameterValue::Quoted(inner));
    }

    match !value.is_empty() && value.bytes().all(is_token_byte) {
        true => Ok(ParameterValue::Unquoted(value)),
        false => Err(malformed("invalid multipart unquoted parameter")),
    }
}

fn parse_parameter(segment: &str) -> Result<HeaderParameter<'_>, RuntimeError> {
    let (key, value) = segment
        .split_once('=')
        .ok_or_else(|| malformed(INVALID_HEADER_PARAMETER))?;
    let key = key.trim();
    let value = value.trim();

    match !key.is_empty() && key.bytes().all(is_token_byte) {
        true => Ok(HeaderParameter {
            name: key,
            value: parse_parameter_value(value)?,
        }),
        false => Err(malformed("invalid multipart parameter name")),
    }
}

/// Store one content-disposition parameter, or name the rule it broke.
///
/// The two failures are stated separately because they are different faults: a
/// header that repeats a parameter and a header that leaves one blank are not
/// the same grammar error, and the refusal carries this text as the operator's
/// only account of which rule fired.
fn set_owned_param_once(
    slot: &mut Option<Box<str>>,
    value: ParameterValue<'_>,
    duplicate: &'static str,
    empty: &'static str,
) -> Result<(), RuntimeError> {
    if slot.is_some() {
        return Err(malformed(duplicate));
    }

    let decoded = value.decode();
    if decoded.is_empty() {
        return Err(malformed(empty));
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
        return Err(malformed(INVALID_QUOTED_PARAMETER));
    }

    let segment = input.trim();
    match segment.is_empty() {
        true => Err(malformed(INVALID_HEADER_PARAMETER)),
        false => Ok((segment, None)),
    }
}

fn parameter_segment<'a>(
    segment: &'a str,
    remaining: &'a str,
) -> Result<(&'a str, Option<&'a str>), RuntimeError> {
    match segment.is_empty() {
        true => Err(malformed(INVALID_HEADER_PARAMETER)),
        false => Ok((segment, Some(remaining))),
    }
}

pub(super) fn find_bytes(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    match from >= haystack.len() || needle.is_empty() {
        true => None,
        false => haystack[from..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|pos| pos + from),
    }
}

pub(super) fn split_header_params(
    header: &str,
) -> Result<(&str, HeaderParameters<'_>), RuntimeError> {
    let (head, remaining) = match header.split_once(';') {
        Some((head, parameters)) => (head.trim(), Some(parameters)),
        None => (header.trim(), None),
    };
    match head.is_empty() {
        true => Err(malformed("invalid multipart header")),
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

/// Whether one boundary satisfies the grammar within a caller's own maximum.
///
/// The maximum is a parameter rather than a constant because a streaming route
/// may configure a stricter one than the protocol's; the protocol maximum is
/// applied by the caller that has no configuration of its own.
pub(super) fn validate_boundary_within(
    boundary: &str,
    max_bytes: usize,
) -> Result<(), RuntimeError> {
    let valid_length = (1..=max_bytes.min(PROTOCOL_MAX_BOUNDARY_BYTES)).contains(&boundary.len());
    let valid_characters = boundary.bytes().all(is_boundary_char);
    let valid_ending = !boundary.ends_with(' ');
    match valid_length && valid_characters && valid_ending {
        true => Ok(()),
        false => Err(malformed(INVALID_BOUNDARY)),
    }
}

/// The boundary one `Content-Type` declares, within a caller's own maximum.
pub(super) fn extract_boundary_within(
    content_type: &str,
    max_bytes: usize,
) -> Result<Cow<'_, str>, RuntimeError> {
    let (media_type, params) = split_header_params(content_type)?;
    if !media_type.eq_ignore_ascii_case("multipart/form-data") {
        return Err(malformed(INVALID_BOUNDARY));
    }

    let mut boundary = None;
    for parameter in params {
        let parameter = parameter?;
        match parameter.name.eq_ignore_ascii_case("boundary") {
            true if boundary.is_some() => {
                return Err(malformed(INVALID_BOUNDARY));
            }
            true => {
                let decoded = parameter.value.decode();
                validate_boundary_within(&decoded, max_bytes)?;
                boundary = Some(decoded);
            }
            false => {}
        }
    }

    boundary.ok_or_else(|| malformed(INVALID_BOUNDARY))
}

/// The boundary one request declared, within a route's configured maximum.
///
/// `None` is a request that declared no representation at all, which declares
/// no boundary either and is refused for the reason a malformed one is: this
/// stage runs before any payload frame is polled, so it is the last point that
/// can tell the peer its framing is unusable without having read it.
pub(in crate::http) fn request_boundary(
    content_type: Option<&str>,
    max_bytes: usize,
) -> Result<Box<str>, RuntimeError> {
    let declared = content_type.ok_or_else(|| malformed(INVALID_BOUNDARY))?;
    extract_boundary_within(declared, max_bytes).map(|boundary| Box::from(boundary.as_ref()))
}

/// What one part's headers establish, as they are read.
///
/// One value the header loop fills, rather than three `&mut` slots and a flag
/// passed alongside them: every field here is decided by the same match, and a
/// caller that holds them separately can hand them over in the wrong order.
#[derive(Default)]
pub(super) struct PartHeaders {
    pub(super) name: Option<Box<str>>,
    pub(super) filename: Option<Box<str>>,
    pub(super) content_type: Option<Box<str>>,
}

impl PartHeaders {
    /// Fold one header line into the values it establishes.
    ///
    /// A repeated `Content-Disposition` is caught by the name already being
    /// set: `parse_content_disposition` fails unless it yields a name, and the
    /// name and the filename are stored together, so the presence of a name IS
    /// the record that the header was seen.
    pub(super) fn absorb(
        &mut self,
        header_name: &str,
        header_value: &str,
    ) -> Result<(), RuntimeError> {
        match (
            header_name.eq_ignore_ascii_case("content-disposition"),
            header_name.eq_ignore_ascii_case("content-type"),
            self.name.is_some(),
            self.content_type.is_some(),
        ) {
            (true, false, true, _) => Err(malformed(INVALID_CONTENT_DISPOSITION)),
            (true, false, false, _) => self.absorb_disposition(header_value),
            (false, true, _, true) => Err(malformed(INVALID_PART_HEADERS)),
            (false, true, _, false) => {
                self.content_type = Some(Box::from(header_value));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Fold one `Content-Disposition` header into the field it names.
    fn absorb_disposition(&mut self, header_value: &str) -> Result<(), RuntimeError> {
        let (disposition, parameters) = split_header_params(header_value)?;
        if !disposition.eq_ignore_ascii_case("form-data") {
            return Err(malformed(INVALID_CONTENT_DISPOSITION));
        }
        self.absorb_disposition_parameters(parameters)?;
        match self.name.is_some() {
            true => Ok(()),
            false => Err(malformed(INVALID_CONTENT_DISPOSITION)),
        }
    }

    /// Store the two content-disposition parameters this grammar recognizes.
    ///
    /// Everything else a disposition may carry is syntactically checked by the
    /// iterator and then ignored, which is what the grammar says to do.
    fn absorb_disposition_parameters(
        &mut self,
        parameters: HeaderParameters<'_>,
    ) -> Result<(), RuntimeError> {
        for parameter in parameters {
            let parameter = parameter?;
            match (
                parameter.name.eq_ignore_ascii_case("name"),
                parameter.name.eq_ignore_ascii_case("filename"),
            ) {
                (true, false) => set_owned_param_once(
                    &mut self.name,
                    parameter.value,
                    "multipart content-disposition repeats the name parameter",
                    "multipart content-disposition names an empty field",
                )?,
                (false, true) => set_owned_param_once(
                    &mut self.filename,
                    parameter.value,
                    "multipart content-disposition repeats the filename parameter",
                    "multipart content-disposition carries an empty filename",
                )?,
                _ => {}
            }
        }
        Ok(())
    }
}

/// Read one part's header block into the values it establishes.
///
/// Both parsers arrive here with the same bytes: everything from the start of
/// the block up to, but not including, its terminating `\r\n\r\n`.
pub(super) fn parse_header_block(raw: &[u8]) -> Result<PartHeaders, RuntimeError> {
    let text = std::str::from_utf8(raw).map_err(|_| malformed(INVALID_PART_HEADERS))?;
    let mut headers = PartHeaders::default();
    for line in text.split("\r\n") {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| malformed(INVALID_PART_HEADERS))?;
        headers.absorb(name.trim(), value.trim())?;
    }
    Ok(headers)
}

/// How many header lines one part's header block declares.
pub(super) fn header_line_count(raw: &[u8]) -> usize {
    match raw.is_empty() {
        true => 0,
        false => raw.windows(2).filter(|window| *window == b"\r\n").count() + 1,
    }
}

/// Whether these bytes open a body framed by this boundary.
///
/// The opening delimiter is the one line with no leading CRLF, and both parsers
/// recognize it here: a prefix one of them accepted and the other refused would
/// be two grammars.
pub(super) fn opens_body(bytes: &[u8], boundary: &[u8]) -> bool {
    bytes
        .strip_prefix(BODY_PREFIX)
        .is_some_and(|rest| rest.starts_with(boundary))
}

/// The bytes one boundary obliges a streaming parser to carry across a frame
/// edge.
pub(super) const fn delimiter_carry_over(boundary_bytes: usize) -> usize {
    boundary_bytes + DELIMITER_CARRY_OVERHEAD
}

/// Whether one delimiter's suffix closes the body, separates two parts, or is
/// no delimiter suffix at all.
///
/// `None` is the search answer: a run that looked like framing and was not is
/// part data, and reporting that costs no refusal.
fn delimiter_suffix_closes(suffix: Option<&[u8]>) -> Option<bool> {
    match suffix {
        Some(b"\r\n") => Some(false),
        Some(b"--") => Some(true),
        _ => None,
    }
}

/// Whether the delimiter suffix at `at` closes the body.
///
/// `false` separates two parts. Both parsers ask this one question, because a
/// delimiter one of them read as closing and the other read as separating would
/// be two grammars.
pub(super) fn delimiter_closes_at(bytes: &[u8], at: usize) -> Result<bool, RuntimeError> {
    delimiter_suffix_closes(bytes.get(at..at + DELIMITER_SUFFIX_BYTES))
        .ok_or_else(|| malformed(INVALID_DELIMITER_FRAMING))
}

/// Whether the bytes after a closing delimiter are a complete ending.
///
/// `Ok(true)` is nothing at all or the one CRLF the grammar allows. `Ok(false)`
/// is a partial CRLF, which only more bytes can decide. Anything else is not an
/// ending the grammar admits, and is refused here.
///
/// Both parsers settle their ending through this, so an epilogue one of them
/// accepted and the other refused would be two grammars. It answers with the
/// same `Result<bool, _>` shape as [`delimiter_closes_at`], because it is the
/// same kind of question about the same framing.
pub(super) fn epilogue_closes(bytes: &[u8]) -> Result<bool, RuntimeError> {
    match bytes {
        b"" | b"\r\n" => Ok(true),
        b"\r" => Ok(false),
        _ => Err(malformed(INVALID_DELIMITER_FRAMING)),
    }
}

/// The position of the next confirmed delimiter at or after `from`.
///
/// A `\r\n--` run whose boundary does not match, or whose two suffix bytes are
/// not both present, is part data: the search resumes past its prefix instead of
/// treating it as framing.
pub(super) fn find_delimiter(body: &[u8], boundary: &[u8], from: usize) -> Option<usize> {
    let mut search_from = from;

    loop {
        let pos = find_bytes(body, DELIMITER_PREFIX, search_from)?;
        let boundary_start = pos + DELIMITER_PREFIX.len();
        let suffix = boundary_start + boundary.len();
        let boundary_matches = body
            .get(boundary_start..suffix)
            .is_some_and(|candidate| candidate == boundary);
        let suffix_bytes = body.get(suffix..suffix + DELIMITER_SUFFIX_BYTES);

        match (boundary_matches, delimiter_suffix_closes(suffix_bytes)) {
            (true, Some(_)) => return Some(pos),
            _ => search_from = boundary_start,
        }
    }
}
