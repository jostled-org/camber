//! The bounds one streaming multipart route reads its payload under.
//!
//! Every value here is structural: how many fields, how many headers, how big
//! one field, one header block, one boundary, one delivered chunk, and one
//! parser buffer may be. The total request-byte maximum is deliberately absent —
//! [`Router::max_request_body`](crate::http::Router::max_request_body),
//! [`HostRouter::max_request_body`](crate::http::HostRouter::max_request_body),
//! and the router's body-admission policy already resolve exactly one effective
//! maximum before any payload frame is polled, and a second setting here would
//! be a second authority over the same request.

use super::grammar::{self, PROTOCOL_MAX_BOUNDARY_BYTES};
use crate::RuntimeError;

/// The default maximum number of fields one multipart body may carry.
const DEFAULT_MAX_FIELDS: usize = 128;

/// The default maximum byte length of one field's data.
const DEFAULT_MAX_FIELD_BYTES: usize = 8 * 1024 * 1024;

/// The default maximum number of header lines one field may declare.
const DEFAULT_MAX_HEADERS_PER_FIELD: usize = 32;

/// The default maximum byte length of one field's whole header block.
const DEFAULT_MAX_HEADER_BYTES_PER_FIELD: usize = 16 * 1024;

/// The default maximum byte length of the request boundary.
const DEFAULT_MAX_BOUNDARY_BYTES: usize = PROTOCOL_MAX_BOUNDARY_BYTES;

/// The default maximum byte length of one delivered chunk.
const DEFAULT_MAX_CHUNK_BYTES: usize = 64 * 1024;

/// The default maximum retained capacity of the incremental parser.
const DEFAULT_MAX_PARSER_BUFFER_BYTES: usize = 128 * 1024;

/// A zero is refused wherever a limit is read, so the message is stated once.
const ZERO_LIMIT: &str = "multipart limits must all be greater than zero";

/// The immutable bounds one streaming multipart route reads under.
///
/// Copyable because every field is a `usize` a validated build already agreed
/// on: handing one to a route registration is handing over seven numbers, not
/// shared state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultipartLimits {
    max_fields: usize,
    max_field_bytes: usize,
    max_headers_per_field: usize,
    max_header_bytes_per_field: usize,
    max_boundary_bytes: usize,
    max_chunk_bytes: usize,
    max_parser_buffer_bytes: usize,
}

impl Default for MultipartLimits {
    fn default() -> Self {
        Self::DEFAULTS
    }
}

/// The defaults satisfy every rule [`MultipartLimitsBuilder::build`] enforces,
/// proved while this crate compiles.
///
/// `Default` hands back a value no builder checked, so without this a later edit
/// to one default could ship a limit set the checked path would have refused.
const _: () = {
    let defaults = MultipartLimits::DEFAULTS;
    assert!(defaults.max_fields > 0);
    assert!(defaults.max_field_bytes > 0);
    assert!(defaults.max_headers_per_field > 0);
    assert!(defaults.max_header_bytes_per_field > 0);
    assert!(defaults.max_boundary_bytes > 0);
    assert!(defaults.max_chunk_bytes > 0);
    assert!(defaults.max_parser_buffer_bytes > 0);
    assert!(defaults.max_boundary_bytes <= PROTOCOL_MAX_BOUNDARY_BYTES);
    assert!(defaults.max_parser_buffer_bytes >= default_required_parser_buffer_bytes());
};

/// The parser buffer the defaults oblige, read through the same two peaks the
/// checked build path reads.
const fn default_required_parser_buffer_bytes() -> usize {
    let defaults = MultipartLimits::DEFAULTS;
    let header = match header_peak(&defaults) {
        Some(peak) => peak,
        None => usize::MAX,
    };
    let chunk = match chunk_peak(&defaults) {
        Some(peak) => peak,
        None => usize::MAX,
    };
    match header > chunk {
        true => header,
        false => chunk,
    }
}

impl MultipartLimits {
    /// The bounds a route reads under when it narrows nothing.
    const DEFAULTS: Self = Self {
        max_fields: DEFAULT_MAX_FIELDS,
        max_field_bytes: DEFAULT_MAX_FIELD_BYTES,
        max_headers_per_field: DEFAULT_MAX_HEADERS_PER_FIELD,
        max_header_bytes_per_field: DEFAULT_MAX_HEADER_BYTES_PER_FIELD,
        max_boundary_bytes: DEFAULT_MAX_BOUNDARY_BYTES,
        max_chunk_bytes: DEFAULT_MAX_CHUNK_BYTES,
        max_parser_buffer_bytes: DEFAULT_MAX_PARSER_BUFFER_BYTES,
    };

    /// Start from the defaults and narrow what a route needs narrowed.
    pub fn builder() -> MultipartLimitsBuilder {
        MultipartLimitsBuilder {
            limits: Self::default(),
        }
    }

    /// The maximum number of fields one body may carry.
    pub fn max_fields(&self) -> usize {
        self.max_fields
    }

    /// The maximum byte length of one field's data.
    pub fn max_field_bytes(&self) -> usize {
        self.max_field_bytes
    }

    /// The maximum number of header lines one field may declare.
    pub fn max_headers_per_field(&self) -> usize {
        self.max_headers_per_field
    }

    /// The maximum byte length of one field's whole header block.
    ///
    /// Header names, colons, values, every line's CRLF, and the terminating
    /// blank CRLF are all charged against this one number, and the complete
    /// `\r\n\r\n` terminator is charged once.
    pub fn max_header_bytes_per_field(&self) -> usize {
        self.max_header_bytes_per_field
    }

    /// The maximum byte length of the request boundary.
    pub fn max_boundary_bytes(&self) -> usize {
        self.max_boundary_bytes
    }

    /// The maximum byte length of one delivered chunk.
    pub fn max_chunk_bytes(&self) -> usize {
        self.max_chunk_bytes
    }

    /// The maximum retained capacity of the incremental parser.
    pub fn max_parser_buffer_bytes(&self) -> usize {
        self.max_parser_buffer_bytes
    }

    /// The bytes one outstanding read may hold on its way to the handler.
    ///
    /// A chunk answer owns at most one chunk; a field answer owns metadata
    /// copied out of a header block that was already bounded. Exactly one of
    /// them is in flight at a time, so the larger of the two bounds all of them.
    /// Derived rather than configured: it is the memory contract stated in the
    /// numbers a route already chose.
    pub fn max_reply_bytes(&self) -> usize {
        self.max_chunk_bytes.max(self.max_header_bytes_per_field)
    }

    /// The bytes the parser must carry across a frame edge to recognize a
    /// delimiter that straddles it.
    pub(super) const fn delimiter_carry_bytes(&self) -> usize {
        grammar::delimiter_carry_over(self.max_boundary_bytes)
    }
}

/// A checked build of [`MultipartLimits`], initialized to the defaults.
///
/// The setters are consuming and share their names with the limits they set, so
/// a registration reads as the list of bounds it chose.
#[derive(Clone, Copy, Debug)]
pub struct MultipartLimitsBuilder {
    limits: MultipartLimits,
}

impl Default for MultipartLimitsBuilder {
    fn default() -> Self {
        MultipartLimits::builder()
    }
}

impl MultipartLimitsBuilder {
    /// Set the maximum number of fields one body may carry.
    pub fn max_fields(mut self, fields: usize) -> Self {
        self.limits.max_fields = fields;
        self
    }

    /// Set the maximum byte length of one field's data.
    pub fn max_field_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_field_bytes = bytes;
        self
    }

    /// Set the maximum number of header lines one field may declare.
    pub fn max_headers_per_field(mut self, headers: usize) -> Self {
        self.limits.max_headers_per_field = headers;
        self
    }

    /// Set the maximum byte length of one field's whole header block.
    pub fn max_header_bytes_per_field(mut self, bytes: usize) -> Self {
        self.limits.max_header_bytes_per_field = bytes;
        self
    }

    /// Set the maximum byte length of the request boundary.
    pub fn max_boundary_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_boundary_bytes = bytes;
        self
    }

    /// Set the maximum byte length of one delivered chunk.
    pub fn max_chunk_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_chunk_bytes = bytes;
        self
    }

    /// Set the maximum retained capacity of the incremental parser.
    pub fn max_parser_buffer_bytes(mut self, bytes: usize) -> Self {
        self.limits.max_parser_buffer_bytes = bytes;
        self
    }

    /// Validate the complete combination, or name the rule it broke.
    ///
    /// The combination is checked rather than each setter, because the parser
    /// buffer must hold whichever of the header peak and the chunk peak is
    /// larger and neither is known until every value is in.
    pub fn build(self) -> Result<MultipartLimits, RuntimeError> {
        let limits = self.limits;
        positive(limits.max_fields)?;
        positive(limits.max_field_bytes)?;
        positive(limits.max_headers_per_field)?;
        positive(limits.max_header_bytes_per_field)?;
        positive(limits.max_boundary_bytes)?;
        positive(limits.max_chunk_bytes)?;
        positive(limits.max_parser_buffer_bytes)?;

        if limits.max_boundary_bytes > PROTOCOL_MAX_BOUNDARY_BYTES {
            return Err(invalid(
                "multipart max_boundary_bytes cannot exceed the protocol maximum of 70",
            ));
        }

        let required = required_parser_buffer_bytes(&limits)?;
        match limits.max_parser_buffer_bytes >= required {
            true => Ok(limits),
            false => Err(invalid(
                "multipart max_parser_buffer_bytes is below the peak the other limits require",
            )),
        }
    }
}

/// The retained capacity the other limits oblige the parser buffer to hold.
///
/// The header peak covers one raw header block beside the metadata copied out of
/// it. The chunk peak covers one right-sized output allocation beside the
/// delimiter carry. Only one of the two is live at a time, so the larger wins.
fn required_parser_buffer_bytes(limits: &MultipartLimits) -> Result<usize, RuntimeError> {
    let header = header_peak(limits)
        .ok_or_else(|| invalid("multipart max_header_bytes_per_field overflows its parser peak"))?;
    let chunk = chunk_peak(limits)
        .ok_or_else(|| invalid("multipart max_chunk_bytes overflows its parser peak"))?;
    Ok(header.max(chunk))
}

/// One raw header block beside the metadata copied out of it.
const fn header_peak(limits: &MultipartLimits) -> Option<usize> {
    limits.max_header_bytes_per_field.checked_mul(2)
}

/// One right-sized output allocation beside the delimiter carry.
const fn chunk_peak(limits: &MultipartLimits) -> Option<usize> {
    limits
        .max_chunk_bytes
        .checked_add(limits.delimiter_carry_bytes())
}

/// Refuse a limit of zero, which admits nothing at all.
fn positive(value: usize) -> Result<(), RuntimeError> {
    match value > 0 {
        true => Ok(()),
        false => Err(invalid(ZERO_LIMIT)),
    }
}

/// One configuration refusal, named as the caller's own argument.
fn invalid(message: &'static str) -> RuntimeError {
    RuntimeError::InvalidArgument(message.into())
}
