//! The incremental multipart parser.
//!
//! One source frame at a time goes in; ordered field metadata and right-sized
//! data chunks come out. Nothing here polls a body, owns a permit, or decides a
//! response: the parser is a state machine over bytes, and the session driver
//! is what feeds it.
//!
//! Everything the parser retains across a frame edge — the header block under
//! construction, the delimiter carry, the metadata copied out of a header block,
//! and the chunk about to leave — is reserved against one
//! [`MultipartBufferBudget`] before it exists.

use super::budget::MultipartBufferBudget;
use super::grammar::{self, INVALID_CONTENT_DISPOSITION, INVALID_DELIMITER_FRAMING, malformed};
use super::limits::MultipartLimits;
use crate::RuntimeError;
use bytes::{Buf, Bytes};

/// A header block that never terminated within its configured maximum.
const HEADER_BLOCK_TOO_LARGE: &str = "multipart field header block exceeds its configured maximum";

/// More header lines than the route configured.
const TOO_MANY_HEADERS: &str = "multipart field declares more headers than the configured maximum";

/// More fields than the route configured.
const TOO_MANY_FIELDS: &str = "multipart body declares more fields than the configured maximum";

/// A part that declares a nested multipart representation.
const NESTED_MULTIPART: &str = "nested multipart parts are not parsed";

/// A body that ended before its terminal framing.
const TRUNCATED_BODY: &str = "multipart body ended before its closing delimiter";

/// The `Content-Type` prefix a nested multipart representation declares.
const NESTED_MULTIPART_PREFIX: &[u8] = b"multipart/";

/// Whether one part's `Content-Type` declares a nested multipart body.
///
/// The question is ten bytes wide and case-insensitive, so it is answered on the
/// value's own bytes: lowercasing the whole value would allocate a copy of it
/// once per field to read the same ten bytes.
fn declares_nested_multipart(value: &str) -> bool {
    let candidate = value.trim_start().as_bytes();
    candidate
        .get(..NESTED_MULTIPART_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(NESTED_MULTIPART_PREFIX))
}

/// One field's bounded, handler-facing description.
///
/// `bytes` is what the metadata costs, so the owner it moves to can take over
/// the reservation the parser made for it instead of measuring the strings
/// again.
pub(super) struct FieldMetadata {
    pub(super) name: Box<str>,
    pub(super) filename: Option<Box<str>>,
    pub(super) content_type: Option<Box<str>>,
    pub(super) bytes: usize,
}

impl FieldMetadata {
    /// The retained bytes one field's metadata strings cost.
    fn measure(name: &str, filename: Option<&str>, content_type: Option<&str>) -> usize {
        name.len() + filename.map_or(0, str::len) + content_type.map_or(0, str::len)
    }
}

/// What one advance of the parser produced.
pub(super) enum ParserEvent {
    /// A field's headers completed. Its metadata reservation is outstanding.
    Field(FieldMetadata),
    /// Data for the current field. Its reservation is outstanding.
    Chunk(Bytes),
    /// The current field's data ended.
    FieldEnd,
    /// Valid terminal framing was consumed through end of body.
    End,
}

impl ParserEvent {
    /// The parser-budget reservation this event's payload carries with it.
    ///
    /// The owner an event moves to owes exactly this release, so the payload is
    /// accounted once: it leaves the parser budget when another owner takes it.
    pub(super) fn reservation(&self) -> usize {
        match self {
            Self::Field(metadata) => metadata.bytes,
            Self::Chunk(chunk) => chunk.len(),
            Self::FieldEnd | Self::End => 0,
        }
    }
}

/// Where in the wire format the parser currently is.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ParserState {
    /// Reading the opening delimiter line.
    Opening,
    /// Reading one field's header block.
    Headers,
    /// Reading one field's data.
    Data,
    /// The closing delimiter was consumed; one CRLF may follow, nothing else.
    Epilogue,
    /// Terminal framing was consumed through end of body.
    Complete,
}

/// What one step of the state machine decided.
enum Step {
    /// An event for the caller.
    Event(ParserEvent),
    /// Progress was made; step again.
    Continue,
    /// The current source frame is spent.
    NeedMore,
}

/// The bounded incremental multipart state machine.
pub(super) struct IncrementalParser {
    limits: MultipartLimits,
    boundary: Box<[u8]>,
    /// Bytes retained across frame edges: the header block under construction,
    /// or the delimiter carry.
    buffer: Vec<u8>,
    budget: MultipartBufferBudget,
    state: ParserState,
    /// How far into `buffer` a fruitless search already reached.
    scanned: usize,
    fields_seen: usize,
    field_bytes: usize,
}

impl IncrementalParser {
    /// Start a parser over one validated boundary and one validated limit set.
    pub(super) fn new(boundary: &str, limits: MultipartLimits) -> Self {
        Self {
            limits,
            boundary: boundary.as_bytes().into(),
            buffer: Vec::new(),
            budget: MultipartBufferBudget::new(limits.max_parser_buffer_bytes()),
            state: ParserState::Opening,
            scanned: 0,
            fields_seen: 0,
            field_bytes: 0,
        }
    }

    /// The capacity the parser is currently holding.
    pub(super) fn retained_bytes(&self) -> usize {
        self.budget.reserved()
    }

    /// The most capacity the parser ever held.
    pub(super) fn peak_bytes(&self) -> usize {
        self.budget.peak()
    }

    /// Release the reservation an event's payload carried out of the parser.
    ///
    /// Called by the owner the payload moved to, so the bytes are accounted
    /// once: they leave the parser budget exactly when another owner takes them.
    pub(super) fn release_delivered(&mut self, bytes: usize) {
        self.budget.release(bytes);
    }

    /// Advance until this source frame produces an event or is spent.
    pub(super) fn advance(
        &mut self,
        source: &mut Bytes,
    ) -> Result<Option<ParserEvent>, RuntimeError> {
        loop {
            match self.step(source)? {
                Step::Event(event) => return Ok(Some(event)),
                Step::Continue => {}
                Step::NeedMore => return Ok(None),
            }
        }
    }

    /// Settle the session at end of body.
    ///
    /// Terminal framing is the only ending this accepts: a body that stopped
    /// mid-field, mid-header, or mid-delimiter is truncated, not finished.
    pub(super) fn finish(&mut self) -> Result<ParserEvent, RuntimeError> {
        match self.state {
            ParserState::Complete => Ok(ParserEvent::End),
            ParserState::Epilogue if self.epilogue_is_closed() => Ok(self.close_session()),
            _ => Err(malformed(TRUNCATED_BODY)),
        }
    }

    /// Whether the retained bytes are a complete ending.
    ///
    /// At end of body an invalid epilogue and a partial one are the same answer:
    /// the body stopped somewhere the grammar does not end, so `finish` reports
    /// truncation for both rather than the framing refusal a mid-stream parser
    /// would raise.
    fn epilogue_is_closed(&self) -> bool {
        grammar::epilogue_closes(&self.buffer).unwrap_or(false)
    }

    /// Settle a session whose terminal framing is complete.
    ///
    /// The epilogue's own bytes are released here, so a session that ended clean
    /// leaves the parser holding nothing: retained capacity above zero after this
    /// is a leak, not an epilogue.
    fn close_session(&mut self) -> ParserEvent {
        let retained = self.buffer.len();
        self.consume(retained);
        self.state = ParserState::Complete;
        ParserEvent::End
    }

    /// Take one step of the state machine.
    fn step(&mut self, source: &mut Bytes) -> Result<Step, RuntimeError> {
        match self.state {
            ParserState::Opening => self.step_opening(source),
            ParserState::Headers => self.step_headers(source),
            ParserState::Data => self.step_data(source),
            ParserState::Epilogue => self.step_epilogue(source),
            ParserState::Complete => Ok(Step::Event(ParserEvent::End)),
        }
    }

    /// Read the opening delimiter line, which carries no leading CRLF.
    fn step_opening(&mut self, source: &mut Bytes) -> Result<Step, RuntimeError> {
        let suffix_at = grammar::BODY_PREFIX.len() + self.boundary.len();
        let line = suffix_at + grammar::DELIMITER_SUFFIX_BYTES;
        self.fill(source, line.saturating_sub(self.buffer.len()))?;
        if self.buffer.len() < line {
            return Ok(Step::NeedMore);
        }

        if !grammar::opens_body(&self.buffer, &self.boundary) {
            return Err(malformed(INVALID_DELIMITER_FRAMING));
        }
        let next = self.delimiter_suffix(suffix_at)?;
        self.consume(line);
        self.state = next;
        Ok(Step::Continue)
    }

    /// Read one field's header block and publish what it establishes.
    ///
    /// The search is bounded, not just the fill. A preceding data phase fills to
    /// `max_chunk_bytes` plus carry, so the buffer can already hold more of this
    /// block than the header bound admits; searching the whole buffer would read
    /// a header block the bound refuses.
    fn step_headers(&mut self, source: &mut Bytes) -> Result<Step, RuntimeError> {
        let ceiling = self.limits.max_header_bytes_per_field();
        self.fill(source, ceiling.saturating_sub(self.buffer.len()))?;

        let terminator = grammar::HEADER_TERMINATOR;
        let from = self.scanned.saturating_sub(terminator.len() - 1);
        let searchable = ceiling.min(self.buffer.len());
        let end = match grammar::find_bytes(&self.buffer[..searchable], terminator, from) {
            Some(end) => end,
            None => return self.await_header_terminator(ceiling),
        };

        let metadata = self.read_field_metadata(end)?;
        self.consume(end + terminator.len());
        self.scanned = 0;
        self.field_bytes = 0;
        self.state = ParserState::Data;
        Ok(Step::Event(ParserEvent::Field(metadata)))
    }

    /// Copy one field's metadata out of the header block already in the buffer.
    ///
    /// The reservation is taken against the block's own length before the copy
    /// exists, then narrowed to what the copy actually cost: a metadata value can
    /// never be larger than the bytes it was read from.
    fn read_field_metadata(&mut self, end: usize) -> Result<FieldMetadata, RuntimeError> {
        if grammar::header_line_count(&self.buffer[..end]) > self.limits.max_headers_per_field() {
            return Err(malformed(TOO_MANY_HEADERS));
        }
        self.fields_seen += 1;
        if self.fields_seen > self.limits.max_fields() {
            return Err(malformed(TOO_MANY_FIELDS));
        }

        self.budget.reserve(end)?;
        let headers = grammar::parse_header_block(&self.buffer[..end]).inspect_err(|_| {
            self.budget.release(end);
        })?;
        let metadata = Self::into_metadata(headers).inspect_err(|_| {
            self.budget.release(end);
        })?;
        debug_assert!(
            metadata.bytes <= end,
            "metadata copied out of a header block cannot exceed the block it was read from"
        );
        self.budget.release(end - metadata.bytes.min(end));
        Ok(metadata)
    }

    /// Take one parsed header block into the field it describes.
    fn into_metadata(headers: grammar::PartHeaders) -> Result<FieldMetadata, RuntimeError> {
        let name = headers
            .name
            .ok_or_else(|| malformed(INVALID_CONTENT_DISPOSITION))?;
        let nested = headers
            .content_type
            .as_deref()
            .is_some_and(declares_nested_multipart);
        if nested {
            return Err(malformed(NESTED_MULTIPART));
        }
        let bytes = FieldMetadata::measure(
            &name,
            headers.filename.as_deref(),
            headers.content_type.as_deref(),
        );
        Ok(FieldMetadata {
            name,
            filename: headers.filename,
            content_type: headers.content_type,
            bytes,
        })
    }

    /// Read one field's data, yielding right-sized chunks up to the delimiter.
    fn step_data(&mut self, source: &mut Bytes) -> Result<Step, RuntimeError> {
        let carry = grammar::delimiter_carry_over(self.boundary.len());
        let ceiling = self.limits.max_chunk_bytes() + carry;
        self.fill(source, ceiling.saturating_sub(self.buffer.len()))?;

        let from = self.scanned.saturating_sub(carry);
        match grammar::find_delimiter(&self.buffer, &self.boundary, from) {
            Some(0) => self.close_field(),
            Some(pos) => self.yield_chunk(pos),
            None => self.yield_before_carry(carry),
        }
    }

    /// Wait for a header terminator this frame did not carry, or refuse the
    /// block that will never have one within its bound.
    fn await_header_terminator(&mut self, ceiling: usize) -> Result<Step, RuntimeError> {
        self.scanned = self.buffer.len();
        match self.buffer.len() >= ceiling {
            true => Err(malformed(HEADER_BLOCK_TOO_LARGE)),
            false => Ok(Step::NeedMore),
        }
    }

    /// Yield everything that cannot be part of a delimiter straddling the edge.
    ///
    /// The retained tail is the longest proper prefix of a delimiter run, so a
    /// boundary split across two frames is still recognized as framing rather
    /// than delivered as data.
    fn yield_before_carry(&mut self, carry: usize) -> Result<Step, RuntimeError> {
        self.scanned = self.buffer.len();
        match self.buffer.len().saturating_sub(carry) {
            0 => Ok(Step::NeedMore),
            available => self.yield_chunk(available),
        }
    }

    /// Consume the delimiter at the front of the buffer and end the field.
    fn close_field(&mut self) -> Result<Step, RuntimeError> {
        let suffix_at = grammar::DELIMITER_PREFIX.len() + self.boundary.len();
        let next = self.delimiter_suffix(suffix_at)?;
        self.consume(suffix_at + grammar::DELIMITER_SUFFIX_BYTES);
        self.scanned = 0;
        self.state = next;
        Ok(Step::Event(ParserEvent::FieldEnd))
    }

    /// Hand out at most one right-sized chunk from the front of the buffer.
    ///
    /// The bytes are copied into their own allocation before ownership moves, so
    /// a retained chunk keeps nothing of the transport frame it arrived in. The
    /// budget is unchanged by the copy: the buffer releases exactly what the
    /// chunk takes over.
    fn yield_chunk(&mut self, available: usize) -> Result<Step, RuntimeError> {
        let take = available.min(self.limits.max_chunk_bytes());
        let total = self
            .field_bytes
            .checked_add(take)
            .filter(|total| *total <= self.limits.max_field_bytes())
            .ok_or_else(|| {
                RuntimeError::RequestBodyLimit(
                    format!(
                        "multipart field exceeds its configured maximum of {} bytes",
                        self.limits.max_field_bytes()
                    )
                    .into(),
                )
            })?;
        self.field_bytes = total;

        let chunk = Bytes::copy_from_slice(&self.buffer[..take]);
        self.buffer.drain(..take);
        self.scanned = self.scanned.saturating_sub(take);
        Ok(Step::Event(ParserEvent::Chunk(chunk)))
    }

    /// Accept only the ending the grammar allows after the closing delimiter.
    ///
    /// The retained bytes are held, not released, until the body ends: an
    /// epilogue that grows past what the grammar admits must still be refused,
    /// and `finish` releases them once the ending is clean.
    fn step_epilogue(&mut self, source: &mut Bytes) -> Result<Step, RuntimeError> {
        let probe = grammar::EPILOGUE_PROBE_BYTES;
        self.fill(source, probe.saturating_sub(self.buffer.len()))?;
        grammar::epilogue_closes(&self.buffer)?;
        Ok(Step::NeedMore)
    }

    /// Read the bytes that say whether a delimiter separates or closes.
    fn delimiter_suffix(&self, at: usize) -> Result<ParserState, RuntimeError> {
        match grammar::delimiter_closes_at(&self.buffer, at)? {
            true => Ok(ParserState::Epilogue),
            false => Ok(ParserState::Headers),
        }
    }

    /// Take up to `want` more bytes from the source frame into the buffer.
    fn fill(&mut self, source: &mut Bytes, want: usize) -> Result<(), RuntimeError> {
        let take = want.min(source.len());
        if take == 0 {
            return Ok(());
        }
        self.budget.reserve(take)?;
        self.buffer.extend_from_slice(&source[..take]);
        source.advance(take);
        Ok(())
    }

    /// Drop `count` framing bytes off the front of the buffer.
    fn consume(&mut self, count: usize) {
        self.buffer.drain(..count);
        self.budget.release(count);
        self.scanned = self.scanned.saturating_sub(count);
    }
}
