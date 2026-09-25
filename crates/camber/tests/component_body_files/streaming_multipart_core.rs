//! The exact incremental parser and session contract, driven through the
//! production driver by a controlled body.
//!
//! Every case here enters the same generic session code a served route will
//! instantiate. The harness supplies frames and scheduling; it chooses no parser
//! state, no budget, no terminal summary, and no refusal.

use crate::deterministic::{DeterministicCase, DeterministicGenerator};
use crate::streaming_multipart as fixture;

use camber::RuntimeError;
use camber::http::mock::{
    self, MultipartObservation, MultipartOutcome, MultipartSession, MultipartTerminalKind,
};
use camber::http::{MultipartField, MultipartLimits, MultipartStream};
use std::collections::BTreeSet;
use std::future::Future;
use std::io::Write;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// The boundary every case in this module frames its bodies with.
const BOUNDARY: &str = "Bnd9";

/// One part, written as the exact header block and data bytes it puts on the
/// wire.
///
/// Raw rather than built from named parameters because half these cases are
/// about header spellings the grammar must accept or refuse.
struct Part<'a> {
    headers: &'a str,
    data: &'a [u8],
}

/// Frame one multipart body exactly as a peer would send it.
///
/// The framing itself belongs to the shared fixture, so a case here and a case
/// on a served route cannot come to disagree about how a part is delimited.
fn build(boundary: &str, parts: &[Part<'_>], epilogue: &[u8]) -> Vec<u8> {
    frame_raw(
        boundary,
        parts.iter().map(|part| (part.headers, part.data)),
        epilogue,
    )
}

/// Hand raw header blocks and data to the shared fixture's framing.
fn frame_raw<'a>(
    boundary: &str,
    parts: impl Iterator<Item = (&'a str, &'a [u8])>,
    epilogue: &[u8],
) -> Vec<u8> {
    let raw: Box<[(&str, &[u8])]> = parts.collect();
    fixture::raw_multipart_body(boundary, &raw, epilogue)
}

/// The bytes of the opening delimiter line `--boundary\r\n`.
fn opening_bytes(boundary: &str) -> usize {
    2 + boundary.len() + 2
}

/// The structural numbers one row reads under, before a boundary fixes the
/// delimiter carry they oblige.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Bounds {
    fields: usize,
    field_bytes: usize,
    headers: usize,
    header_bytes: usize,
    chunk: usize,
}

impl Bounds {
    /// These bounds under one boundary, with the parser buffer at exactly the
    /// peak they require.
    ///
    /// Tight rather than generous: a buffer with slack would hide an accounting
    /// error the bound is supposed to catch.
    fn within(self, boundary: &str) -> MultipartLimits {
        let required = (2 * self.header_bytes).max(self.chunk + boundary.len() + 5);
        MultipartLimits::builder()
            .max_fields(self.fields)
            .max_field_bytes(self.field_bytes)
            .max_headers_per_field(self.headers)
            .max_header_bytes_per_field(self.header_bytes)
            .max_boundary_bytes(boundary.len())
            .max_chunk_bytes(self.chunk)
            .max_parser_buffer_bytes(required)
            .build()
            .expect("the row's limits are a valid combination")
    }
}

/// One row's limits under this module's boundary.
fn limits(
    fields: usize,
    field_bytes: usize,
    headers: usize,
    header_bytes: usize,
    chunk: usize,
) -> MultipartLimits {
    Bounds {
        fields,
        field_bytes,
        headers,
        header_bytes,
        chunk,
    }
    .within(BOUNDARY)
}

/// Limits generous enough that only the row's own subject can refuse anything.
fn permissive(chunk: usize) -> MultipartLimits {
    limits(64, 1 << 20, 16, 1024, chunk)
}

/// What one field turned out to be.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Collected {
    name: Box<str>,
    filename: Option<Box<str>>,
    content_type: Option<Box<str>>,
    data: Vec<u8>,
}

impl Collected {
    /// The field's name, filename, and content type, without its data.
    fn metadata(&self) -> (&str, Option<&str>, Option<&str>) {
        (
            &self.name,
            self.filename.as_deref(),
            self.content_type.as_deref(),
        )
    }
}

/// What one complete read of a session produced.
#[derive(Default)]
struct Reading {
    fields: Vec<Collected>,
    chunks: Vec<usize>,
    error: Option<RuntimeError>,
}

impl Reading {
    /// Every field name in the order the wire carried them.
    fn names(&self) -> Vec<&str> {
        self.fields.iter().map(|field| &*field.name).collect()
    }

    /// How many payload bytes reached application code.
    fn delivered(&self) -> usize {
        self.fields.iter().map(|field| field.data.len()).sum()
    }
}

/// Read one session to its end, or to the first failure.
///
/// A missing handle is a fault in this harness, not an answer from production:
/// it panics rather than reporting a refusal no production code produced, which
/// a row matching on `RuntimeError::Multipart` would otherwise accept.
async fn read_all(session: &mut MultipartSession) -> Reading {
    let stream = session
        .stream()
        .expect("the case still holds the session's access handle");
    bounded(
        read_stream(stream),
        "a session reads to its end or its failure",
    )
    .await
}

/// Read one access handle to its end, or to the first failure.
async fn read_stream(stream: &mut MultipartStream) -> Reading {
    let mut reading = Reading::default();
    loop {
        let field = match stream.next_field().await {
            Err(error) => {
                reading.error = Some(error);
                return reading;
            }
            Ok(None) => return reading,
            Ok(Some(field)) => field,
        };
        if let Some(error) = read_field(field, &mut reading).await {
            reading.error = Some(error);
            return reading;
        }
    }
}

/// Read one field's chunks into the running result.
async fn read_field(mut field: MultipartField<'_>, reading: &mut Reading) -> Option<RuntimeError> {
    let mut collected = Collected {
        name: field.name().into(),
        filename: field.filename().map(Box::from),
        content_type: field.content_type().map(Box::from),
        data: Vec::new(),
    };
    loop {
        match field.next_chunk().await {
            Err(error) => {
                reading.fields.push(collected);
                return Some(error);
            }
            Ok(None) => {
                reading.fields.push(collected);
                return None;
            }
            Ok(Some(chunk)) => {
                reading.chunks.push(chunk.len());
                collected.data.extend_from_slice(&chunk);
            }
        }
    }
}

/// Poll one held operation exactly once, without waking anything.
///
/// A case that must stand between two phases of the command protocol needs a
/// turn it controls; awaiting would hand the operation its whole lifetime.
fn poll_once<F: Future>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

/// Let every other ready task run to its next parking point.
///
/// The runtime is single-threaded, so this is a scheduling barrier and not a
/// wait: nothing here sleeps or races a clock.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// How long any rendezvous with the driver task may take before the case fails.
///
/// Generous, because it is never reached by a working driver: it exists so a
/// driver that never terminates or never publishes fails this executable
/// instead of hanging it.
const BOUND: Duration = Duration::from_secs(5);

/// How long a negative claim must keep holding before a case accepts it.
const HOLD: Duration = Duration::from_millis(100);

/// Wait for one rendezvous with the driver, and fail the case if it never
/// arrives.
async fn bounded<F: Future>(operation: F, claim: &str) -> F::Output {
    match tokio::time::timeout(BOUND, operation).await {
        Ok(value) => value,
        Err(_) => panic!("{claim}: nothing arrived within {BOUND:?}"),
    }
}

/// Join one driver under the module's bound and report its terminal summary.
async fn finish(session: MultipartSession) -> MultipartOutcome {
    bounded(session.finish(), "the driver joins")
        .await
        .unwrap_or_else(|error| panic!("the driver joins: {error}"))
}

/// Wait under the module's bound until the driver has published `count`
/// replies.
async fn wait_for_replies(session: &MultipartSession, count: usize) {
    let claim = format!("the driver publishes {count} replies");
    bounded(session.wait_for_replies(count), &claim).await;
}

/// Require one monotonic observation to reach the value a claim names and stay
/// there.
///
/// A claim that a counter stopped reads true both when it holds and when the
/// driver has simply not run yet, so [`settle`] cannot carry one: a fixed yield
/// budget that expires early reports the regression as an absence. This waits
/// for the value under [`BOUND`] and then keeps reading it for [`HOLD`],
/// failing the moment the counter passes it. A driver that accepts a retracted
/// command, or keeps reading for a command it can no longer answer, fails here
/// instead of going unseen.
async fn assert_settles_at(
    session: &MultipartSession,
    read: fn(&MultipartObservation) -> usize,
    expected: usize,
    claim: &str,
) {
    let reached = async {
        loop {
            let observed = read(&session.observed());
            assert!(
                observed <= expected,
                "{claim}: the observation ran past {expected} to {observed}"
            );
            if observed == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    };
    bounded(reached, claim).await;

    let moved = tokio::time::timeout(HOLD, async {
        loop {
            let observed = read(&session.observed());
            if observed != expected {
                return observed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Ok(observed) = moved {
        panic!("{claim}: the observation moved from {expected} to {observed}");
    }
}

/// The canonical body every ordering case reads.
fn ordered_parts() -> (Vec<u8>, Vec<Collected>) {
    let binary: &[u8] = b"\x00\x01\r\n--Bnd9xy tail\xff\xfe\r\n";
    let parts = [
        Part {
            headers: "Content-Disposition: form-data; name=\"alpha\"",
            data: b"one",
        },
        Part {
            headers: "Content-Disposition: form-data; name=\"file\"; filename=\"a\\\"b .txt\"\r\nContent-Type: application/octet-stream",
            data: binary,
        },
        Part {
            headers: "Content-Disposition: form-data; name=\"empty\"",
            data: b"",
        },
        Part {
            headers: "Content-Disposition: form-data; name=\"alpha\"",
            data: b"two",
        },
    ];
    let expected = vec![
        Collected {
            name: "alpha".into(),
            filename: None,
            content_type: None,
            data: b"one".to_vec(),
        },
        Collected {
            name: "file".into(),
            filename: Some("a\"b .txt".into()),
            content_type: Some("application/octet-stream".into()),
            data: binary.to_vec(),
        },
        Collected {
            name: "empty".into(),
            filename: None,
            content_type: None,
            data: Vec::new(),
        },
        Collected {
            name: "alpha".into(),
            filename: None,
            content_type: None,
            data: b"two".to_vec(),
        },
    ];
    (build(BOUNDARY, &parts, b""), expected)
}

#[tokio::test]
async fn incremental_multipart_parser_yields_ordered_fields_across_every_split() {
    let (body, expected) = ordered_parts();
    let row = limits(8, 4096, 4, 256, 8);

    every_split_yields_the_same_fields(&body, &expected, row).await;
    terminal_framing_decides_the_ending(row).await;
    strict_content_disposition_refuses_every_other_spelling(row).await;
}

/// Every frame split of one canonical body yields the same ordered fields.
async fn every_split_yields_the_same_fields(
    body: &[u8],
    expected: &[Collected],
    limits: MultipartLimits,
) {
    for split in 1..=body.len() {
        let mut session = mock::multipart_session(BOUNDARY, limits)
            .frames_of(body, split)
            .start();
        let reading = read_all(&mut session).await;
        assert!(
            reading.error.is_none(),
            "split {split} must parse cleanly, got {:?}",
            reading.error
        );
        assert_eq!(
            reading.fields.as_slice(),
            expected,
            "split {split} changed the fields"
        );
        assert!(
            reading.chunks.iter().all(|size| *size > 0 && *size <= 8),
            "split {split} yielded a chunk outside 1..=8: {:?}",
            reading.chunks
        );
        let outcome = finish(session).await;
        assert_eq!(
            outcome.terminal(),
            MultipartTerminalKind::Clean,
            "split {split} must end cleanly"
        );
        assert!(
            outcome.observed().parser_peak_bytes() <= limits.max_parser_buffer_bytes(),
            "split {split} held {} parser bytes",
            outcome.observed().parser_peak_bytes()
        );
    }
}

/// Nothing but end of body or one CRLF may follow the closing delimiter.
async fn terminal_framing_decides_the_ending(limits: MultipartLimits) {
    let epilogue_rows: [(&[u8], MultipartTerminalKind); 4] = [
        (b"", MultipartTerminalKind::Clean),
        (b"\r\n", MultipartTerminalKind::Clean),
        (b"\r\n\r\n", MultipartTerminalKind::Structural),
        (b"trailing", MultipartTerminalKind::Structural),
    ];
    for (epilogue, expected_terminal) in epilogue_rows {
        let framed = build(
            BOUNDARY,
            &[Part {
                headers: "Content-Disposition: form-data; name=\"alpha\"",
                data: b"one",
            }],
            epilogue,
        );
        let mut session = mock::multipart_session(BOUNDARY, limits)
            .frames_of(&framed, 7)
            .start();
        let reading = read_all(&mut session).await;
        if expected_terminal == MultipartTerminalKind::Clean {
            assert!(
                reading.error.is_none(),
                "epilogue {epilogue:?} must read cleanly, got {:?}",
                reading.error
            );
            assert_eq!(
                reading.names(),
                vec!["alpha"],
                "epilogue {epilogue:?} must still deliver the body's one field"
            );
        }
        let outcome = finish(session).await;
        assert_eq!(
            outcome.terminal(),
            expected_terminal,
            "epilogue {epilogue:?} must settle as {expected_terminal:?}"
        );
    }
}

/// The grammar decides a content disposition, not the header's presence.
async fn strict_content_disposition_refuses_every_other_spelling(limits: MultipartLimits) {
    let disposition_rows = [
        "Content-Disposition: attachment; name=\"a\"",
        "Content-Disposition: form-data",
        "Content-Disposition: form-data; name=\"\"",
        "Content-Disposition: form-data; name=\"a\"; name=\"b\"",
        "Content-Disposition: form-data; name=\"a\"\r\nContent-Disposition: form-data; name=\"b\"",
        "X-Only: value",
    ];
    for headers in disposition_rows {
        let framed = build(
            BOUNDARY,
            &[Part {
                headers,
                data: b"one",
            }],
            b"",
        );
        let mut session = mock::multipart_session(BOUNDARY, limits)
            .frames_of(&framed, 9)
            .start();
        let reading = read_all(&mut session).await;
        assert!(
            reading.error.is_some(),
            "`{headers}` must be refused by the grammar"
        );
        let outcome = finish(session).await;
        assert_eq!(
            outcome.terminal(),
            MultipartTerminalKind::Structural,
            "`{headers}` must settle as a structural failure"
        );
    }
}

#[tokio::test]
async fn streaming_multipart_budget_rejects_crossings_before_retention() {
    let big: Vec<u8> = (0..600u32).map(|byte| byte as u8).collect();
    let two_parts = [
        Part {
            headers: "Content-Disposition: form-data; name=\"alpha\"\r\nContent-Type: text/plain",
            data: &big,
        },
        Part {
            headers: "Content-Disposition: form-data; name=\"beta\"",
            data: b"tail",
        },
    ];
    let body = build(BOUNDARY, &two_parts, b"");
    let header_block = "Content-Disposition: form-data; name=\"alpha\"\r\nContent-Type: text/plain";
    let header_bytes = header_block.len() + 4;

    admitted_total_refuses_the_crossing_frame(&body, big.len()).await;
    field_bytes_refuse_the_crossing_field(&body, big.len()).await;
    structural_bounds_refuse_their_crossings(&body, header_bytes).await;
    every_refusal_keeps_its_own_provenance(&body, &two_parts).await;
    saturating_bounds_admit_every_frame(&body, big.len()).await;
}

/// The admitted total is the one authority over how much body is read.
async fn admitted_total_refuses_the_crossing_frame(body: &[u8], field_len: usize) {
    let mut session = mock::multipart_session(BOUNDARY, permissive(64))
        .frames_of(body, 32)
        .body_limit(body.len())
        .start();
    let reading = read_all(&mut session).await;
    assert!(
        reading.error.is_none(),
        "a body exactly at its maximum is admitted"
    );
    assert_eq!(reading.delivered(), field_len + 4);
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Clean);

    let mut session = mock::multipart_session(BOUNDARY, permissive(64))
        .frames_of(body, 32)
        .body_limit(body.len() - 1)
        .with_permit()
        .start();
    let reading = read_all(&mut session).await;
    assert!(
        matches!(reading.error, Some(RuntimeError::RequestBodyLimit(_))),
        "a crossing total must report byte-limit provenance, got {:?}",
        reading.error
    );
    assert!(
        reading.delivered() < field_len + 4,
        "the crossing frame's bytes must never reach application code"
    );
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::ByteLimit);
    assert_eq!(
        outcome.observed().permit_owners_dropped(),
        1,
        "a refused session releases its admitted permit exactly once"
    );
}

/// Per-field bytes: exactly at the maximum, then one byte under it.
async fn field_bytes_refuse_the_crossing_field(body: &[u8], field_len: usize) {
    for (field_bytes, expected) in [
        (field_len, MultipartTerminalKind::Clean),
        (field_len - 1, MultipartTerminalKind::ByteLimit),
    ] {
        let row = limits(8, field_bytes, 4, 256, 64);
        let mut session = mock::multipart_session(BOUNDARY, row)
            .frames_of(body, 48)
            .start();
        let reading = read_all(&mut session).await;
        let outcome = finish(session).await;
        assert_eq!(
            outcome.terminal(),
            expected,
            "a field bound of {field_bytes} must settle as {expected:?}"
        );
        if expected == MultipartTerminalKind::Clean {
            assert!(
                reading.error.is_none(),
                "a field exactly at its bound is read, got {:?}",
                reading.error
            );
            assert_eq!(
                reading.delivered(),
                field_bytes + 4,
                "a field bound of {field_bytes} admits every byte both fields carry"
            );
        } else {
            assert!(
                reading.delivered() <= field_bytes + 4,
                "no crossing field byte reaches application code"
            );
        }
    }
}

/// Field count, header count, and header bytes: at the bound, then over it.
async fn structural_bounds_refuse_their_crossings(body: &[u8], header_bytes: usize) {
    let structural_rows: [(&str, MultipartLimits, MultipartTerminalKind); 6] = [
        (
            "fields at the bound",
            limits(2, 1 << 20, 4, 256, 64),
            MultipartTerminalKind::Clean,
        ),
        (
            "fields over the bound",
            limits(1, 1 << 20, 4, 256, 64),
            MultipartTerminalKind::Structural,
        ),
        (
            "headers at the bound",
            limits(8, 1 << 20, 2, 256, 64),
            MultipartTerminalKind::Clean,
        ),
        (
            "headers over the bound",
            limits(8, 1 << 20, 1, 256, 64),
            MultipartTerminalKind::Structural,
        ),
        (
            "header bytes at the bound",
            limits(8, 1 << 20, 4, header_bytes, 64),
            MultipartTerminalKind::Clean,
        ),
        (
            "header bytes over the bound",
            limits(8, 1 << 20, 4, header_bytes - 1, 64),
            MultipartTerminalKind::Structural,
        ),
    ];
    for (label, row, expected) in structural_rows {
        let mut session = mock::multipart_session(BOUNDARY, row)
            .frames_of(body, 37)
            .start();
        let reading = read_all(&mut session).await;
        if expected == MultipartTerminalKind::Clean {
            assert!(
                reading.error.is_none(),
                "{label} must read cleanly, got {:?}",
                reading.error
            );
            assert_eq!(
                reading.names(),
                vec!["alpha", "beta"],
                "{label} must still deliver both fields"
            );
        }
        let outcome = finish(session).await;
        assert_eq!(outcome.terminal(), expected, "{label}");
        assert!(
            outcome.observed().parser_peak_bytes() <= row.max_parser_buffer_bytes(),
            "{label} held {} parser bytes above its {} maximum",
            outcome.observed().parser_peak_bytes(),
            row.max_parser_buffer_bytes()
        );
    }
}

/// Chunk sizing, truncation, a wrong opening boundary, and an unreadable
/// transport each report their own provenance.
async fn every_refusal_keeps_its_own_provenance(body: &[u8], parts: &[Part<'_>]) {
    let row = limits(8, 1 << 20, 4, 256, 17);
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 64)
        .start();
    let reading = read_all(&mut session).await;
    let carried: usize = parts.iter().map(|part| part.data.len()).sum();
    assert!(
        reading.error.is_none(),
        "a chunk bound alone refuses nothing, got {:?}",
        reading.error
    );
    assert_eq!(
        reading.delivered(),
        carried,
        "every payload byte both parts carry reaches application code"
    );
    assert!(
        !reading.chunks.is_empty(),
        "a right-sized chunk claim is about chunks that exist"
    );
    assert!(
        reading.chunks.iter().all(|size| *size > 0 && *size <= 17),
        "every chunk is right-sized: {:?}",
        reading.chunks
    );
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Clean
    );

    let mut session = mock::multipart_session(BOUNDARY, permissive(64))
        .frames_of(&body[..body.len() - 6], 40)
        .start();
    assert!(
        read_all(&mut session).await.error.is_some(),
        "truncation is refused"
    );
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Structural
    );

    let wrong = build("Other7", parts, b"");
    let mut session = mock::multipart_session(BOUNDARY, permissive(64))
        .frames_of(&wrong, 40)
        .start();
    assert!(
        read_all(&mut session).await.error.is_some(),
        "a body framed with another boundary is refused"
    );
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Structural
    );

    let mut session = mock::multipart_session(BOUNDARY, permissive(64))
        .frames_of(&body[..64], 32)
        .transport_failure("peer reset the upload")
        .with_permit()
        .start();
    let reading = read_all(&mut session).await;
    assert!(
        matches!(reading.error, Some(RuntimeError::RequestBodyUnreadable(_))),
        "a transport failure keeps its own provenance, got {:?}",
        reading.error
    );
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Unreadable);
    assert_eq!(outcome.observed().permit_owners_dropped(), 1);
}

/// A maximum that cannot overflow still reads a body correctly.
async fn saturating_bounds_admit_every_frame(body: &[u8], field_len: usize) {
    let mut session = mock::multipart_session(BOUNDARY, limits(8, usize::MAX, 4, 256, 64))
        .frames_of(body, 29)
        .body_limit(usize::MAX)
        .start();
    let reading = read_all(&mut session).await;
    assert!(
        reading.error.is_none(),
        "saturating bounds admit every frame"
    );
    assert_eq!(reading.delivered(), field_len + 4);
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Clean
    );
}

#[tokio::test]
async fn streaming_multipart_memory_high_water_covers_active_metadata_and_pending_reply() {
    let headers = "Content-Disposition: form-data; name=\"n\"; filename=\"a-long-upload-name-that-costs-real-metadata-bytes.bin\"\r\nContent-Type: application/octet-stream";
    let metadata_bytes = 1
        + "a-long-upload-name-that-costs-real-metadata-bytes.bin".len()
        + "application/octet-stream".len();
    let header_bytes = headers.len() + 4;
    let chunk_bytes = 64;
    assert!(
        header_bytes > chunk_bytes,
        "this case configures header bytes above chunk bytes"
    );
    let row = limits(4, 1 << 20, 4, header_bytes, chunk_bytes);
    assert_eq!(row.max_reply_bytes(), header_bytes);

    let payload: Vec<u8> = (0..4096u32).map(|byte| byte as u8).collect();
    let body = build(
        BOUNDARY,
        &[Part {
            headers,
            data: &payload,
        }],
        b"",
    );
    let head = opening_bytes(BOUNDARY) + header_bytes;

    let mut session = mock::multipart_session(BOUNDARY, row)
        .frame(&body[..head])
        .frame(&body[head..])
        .start();
    let mut stream = session.take_stream().expect("the session holds its handle");

    let field = pending_metadata_transfers_out_of_the_parser(
        &mut session,
        &mut stream,
        row,
        metadata_bytes,
    )
    .await;
    let retained =
        pending_chunk_is_held_beside_the_active_metadata(&mut session, field, row, chunk_bytes)
            .await;
    assert_eq!(
        retained,
        payload.len(),
        "the whole field reached application code"
    );

    assert!(
        stream.next_field().await.expect("the body ends").is_none(),
        "the body ends after its one field"
    );
    drop(stream);
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Clean);

    let observed = outcome.observed();
    assert!(
        observed.parser_peak_bytes() <= row.max_parser_buffer_bytes(),
        "parser retention stayed bounded while the field grew to {} bytes",
        payload.len()
    );
    assert!(observed.reply_peak_bytes() <= row.max_reply_bytes());
    assert!(observed.active_metadata_peak_bytes() <= row.max_header_bytes_per_field());
    assert!(
        payload.len() > row.max_parser_buffer_bytes() + row.max_reply_bytes(),
        "the field is far larger than every framework bound this case asserts"
    );
}

/// Hold the maximum metadata pending, then acknowledge it into an active field.
///
/// The reservation must move from the parser budget to the pending reply and
/// then to the active field, never counted in two places at once.
async fn pending_metadata_transfers_out_of_the_parser<'a>(
    session: &mut MultipartSession,
    stream: &'a mut MultipartStream,
    row: MultipartLimits,
    metadata_bytes: usize,
) -> MultipartField<'a> {
    let mut pending_field = Box::pin(stream.next_field());
    assert!(
        poll_once(&mut pending_field).is_pending(),
        "the command is in flight"
    );
    wait_for_replies(session, 1).await;

    let held = session.observed();
    assert_eq!(
        held.parser_retained_bytes(),
        0,
        "the metadata reservation transferred out of the parser instead of duplicating"
    );
    assert_eq!(
        held.reply_retained_bytes(),
        metadata_bytes,
        "the pending reply owns exactly the metadata it published"
    );
    assert!(
        held.reply_peak_bytes() <= row.max_reply_bytes(),
        "a pending reply stays within its derived bound"
    );

    let field = pending_field
        .await
        .expect("the held command answers with a field")
        .expect("the body has a field");
    settle().await;
    let active = session.observed();
    assert_eq!(
        active.reply_retained_bytes(),
        0,
        "the acknowledged metadata left reply accounting"
    );
    assert!(
        active.active_metadata_peak_bytes() >= metadata_bytes,
        "the active field's metadata is a mandatory session term"
    );
    field
}

/// Hold one maximum chunk reply pending while the active field is alive, then
/// read the field to its end. Reports how many payload bytes it delivered.
async fn pending_chunk_is_held_beside_the_active_metadata(
    session: &mut MultipartSession,
    mut field: MultipartField<'_>,
    row: MultipartLimits,
    chunk_bytes: usize,
) -> usize {
    let mut pending_chunk = Box::pin(field.next_chunk());
    assert!(poll_once(&mut pending_chunk).is_pending());
    wait_for_replies(session, 2).await;
    let both = session.observed();
    assert_eq!(
        both.reply_retained_bytes(),
        chunk_bytes,
        "a maximum chunk reply is held pending beside the active metadata"
    );
    assert!(both.reply_peak_bytes() <= row.max_reply_bytes());
    assert!(both.active_metadata_peak_bytes() <= row.max_header_bytes_per_field());
    assert!(both.parser_peak_bytes() <= row.max_parser_buffer_bytes());

    let first = pending_chunk
        .await
        .expect("the held chunk arrives")
        .expect("the field has data");
    assert_eq!(first.len(), chunk_bytes);

    let mut retained = vec![first];
    while let Some(chunk) = field
        .next_chunk()
        .await
        .expect("the field reads to its end")
    {
        retained.push(chunk);
    }
    drop(field);
    retained.iter().map(|chunk| chunk.len()).sum()
}

#[tokio::test]
async fn yielded_chunks_release_source_backing_before_application_retention() {
    let payload: Vec<u8> = (0..2048u32).map(|byte| byte as u8).collect();
    let body = build(
        BOUNDARY,
        &[Part {
            headers: "Content-Disposition: form-data; name=\"one\"",
            data: &payload,
        }],
        b"",
    );
    let row = limits(4, 1 << 20, 4, 256, 64);

    // One oversized source frame carries the whole body.
    let mut session = mock::multipart_session(BOUNDARY, row).frame(&body).start();
    let mut stream = session.take_stream().expect("the session holds its handle");
    let mut field = stream
        .next_field()
        .await
        .expect("the field parses")
        .expect("the body has one field");

    let mut retained = Vec::new();
    while let Some(chunk) = field
        .next_chunk()
        .await
        .expect("the field reads to its end")
    {
        assert!(!chunk.is_empty(), "no yielded chunk is empty");
        assert!(chunk.len() <= 64, "no yielded chunk exceeds its bound");
        retained.push(chunk);
    }
    drop(field);

    assert_eq!(
        session.observed().source_frame_backings_freed(),
        Some(1),
        "the exhausted source frame released its backing while every chunk is still live"
    );
    assert_eq!(
        retained.len(),
        payload.len().div_ceil(64),
        "the field arrived as right-sized chunks"
    );
    let joined: Vec<u8> = retained
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect();
    assert_eq!(joined, payload, "the chunks are the field, in order");

    drop(retained);
    assert!(
        stream.next_field().await.expect("the body ends").is_none(),
        "the body ends after its one field"
    );
    drop(stream);
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Clean);
    assert_eq!(outcome.observed().source_frame_backings_freed(), Some(1));
    assert_eq!(
        outcome.observed().body_frames_polled(),
        1,
        "the driver never held a second source frame"
    );
}

#[tokio::test]
async fn multipart_command_cancellation_and_discard_are_deterministic() {
    let payload: Vec<u8> = (0..512u32).map(|byte| byte as u8).collect();
    let parts = [
        Part {
            headers: "Content-Disposition: form-data; name=\"alpha\"",
            data: &payload,
        },
        Part {
            headers: "Content-Disposition: form-data; name=\"beta\"",
            data: b"tail",
        },
    ];
    let body = build(BOUNDARY, &parts, b"");
    let row = limits(4, 1 << 20, 4, 256, 32);

    cancelling_before_acceptance_leaves_no_trace(&body, row).await;
    cancelling_after_ingress_abandons_and_stops_polling(&body, row).await;
    cancelling_after_publication_abandons(&body, row).await;
    a_cancelled_discard_is_terminal_at_every_phase(&body, row).await;
    a_completed_discard_permits_exactly_the_next_field(&body, row).await;
    dropping_an_incomplete_field_poisons_the_stream(&body, row).await;
    revocation_closes_admission_from_outside_the_handle(&body, row).await;
}

/// Cancelled before acceptance: no ingress, no state change, no trace.
async fn cancelling_before_acceptance_leaves_no_trace(body: &[u8], row: MultipartLimits) {
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 24)
        .start();
    {
        let stream = session.stream().expect("the session holds its handle");
        let mut pending = Box::pin(stream.next_field());
        assert!(poll_once(&mut pending).is_pending());
        drop(pending);
    }
    assert_settles_at(
        &session,
        MultipartObservation::commands_accepted,
        0,
        "an unaccepted command is retracted",
    )
    .await;
    assert_settles_at(
        &session,
        MultipartObservation::body_frames_polled,
        0,
        "no ingress ran",
    )
    .await;
    let reading = read_all(&mut session).await;
    assert!(reading.error.is_none(), "the session is untouched");
    assert_eq!(reading.names(), vec!["alpha", "beta"]);
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Clean
    );
}

/// Cancelled after ingress advanced but before publication.
///
/// The frames that command needed: the first field's metadata ends inside the
/// third 24-byte frame, and the whole body is 27 of them. Naming both is what
/// makes "polling stops" falsifiable — a driver that kept reading for a session
/// it can no longer answer would run to 27.
async fn cancelling_after_ingress_abandons_and_stops_polling(body: &[u8], row: MultipartLimits) {
    const LOST_COMMAND_FRAMES: usize = 3;
    let framed = body.chunks(24).count();
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 24)
        .stall_after(1)
        .with_permit()
        .start();
    let mut stream = session.take_stream().expect("the session holds its handle");
    {
        let mut pending = Box::pin(stream.next_field());
        assert!(poll_once(&mut pending).is_pending());
        settle().await;
        let mid = session.observed();
        assert_eq!(
            mid.commands_accepted(),
            1,
            "the driver accepted the command"
        );
        assert_eq!(mid.body_frames_polled(), 1, "ingress advanced by one frame");
        assert_eq!(mid.replies_published(), 0, "no reply exists yet");
        drop(pending);
    }
    settle().await;
    // Release the parked body before measuring. While the gate holds it the
    // count is pinned at one whether or not losing the operation stopped
    // anything; with the gate open, only a stopped driver leaves frames unread.
    session.release_body();
    assert_settles_at(
        &session,
        MultipartObservation::body_frames_polled,
        LOST_COMMAND_FRAMES,
        "a lost operation stops ingress at the command it was serving",
    )
    .await;
    assert!(
        LOST_COMMAND_FRAMES < framed,
        "the released body still holds {} frames the stopped driver never read",
        framed - LOST_COMMAND_FRAMES
    );
    assert!(
        matches!(stream.next_field().await, Err(RuntimeError::Multipart(_))),
        "an abandoned session admits no later operation"
    );
    drop(stream);
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Abandoned);
    assert_eq!(
        outcome.observed().body_frames_polled(),
        LOST_COMMAND_FRAMES,
        "joining an abandoned driver reads nothing further"
    );
    assert_eq!(outcome.observed().permit_owners_dropped(), 1);
}

/// Cancelled after publication but before acknowledgment.
async fn cancelling_after_publication_abandons(body: &[u8], row: MultipartLimits) {
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 64)
        .start();
    let mut stream = session.take_stream().expect("the session holds its handle");
    {
        let mut pending = Box::pin(stream.next_field());
        assert!(poll_once(&mut pending).is_pending());
        wait_for_replies(&session, 1).await;
        drop(pending);
    }
    settle().await;
    assert!(
        stream.next_field().await.is_err(),
        "a reply lost before acknowledgment abandons the session"
    );
    drop(stream);
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Abandoned
    );
}

/// A cancelled discard is terminal at every phase, unaccepted included.
async fn a_cancelled_discard_is_terminal_at_every_phase(body: &[u8], row: MultipartLimits) {
    for published in [false, true] {
        let mut session = mock::multipart_session(BOUNDARY, row)
            .frames_of(body, 64)
            .start();
        let mut stream = session.take_stream().expect("the session holds its handle");
        {
            let field = stream
                .next_field()
                .await
                .expect("the field parses")
                .expect("the body has a field");
            let mut pending = Box::pin(field.discard());
            assert!(poll_once(&mut pending).is_pending());
            if published {
                wait_for_replies(&session, 2).await;
            }
            drop(pending);
        }
        drop(stream);
        settle().await;
        assert_eq!(
            finish(session).await.terminal(),
            MultipartTerminalKind::Abandoned,
            "a cancelled discard abandons the session (published: {published})"
        );
    }
}

/// A completed discard drains exactly one field and permits the next.
async fn a_completed_discard_permits_exactly_the_next_field(body: &[u8], row: MultipartLimits) {
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 24)
        .start();
    {
        let stream = session.stream().expect("the session holds its handle");
        let field = stream
            .next_field()
            .await
            .expect("the field parses")
            .expect("the body has a field");
        assert_eq!(field.name(), "alpha");
        field.discard().await.expect("a discard drains one field");
        let next = stream
            .next_field()
            .await
            .expect("the next field parses")
            .expect("the body has a second field");
        assert_eq!(next.name(), "beta", "discard advanced exactly one field");
        next.discard()
            .await
            .expect("the second field discards under the same bounds");
        assert!(
            stream.next_field().await.expect("the body ends").is_none(),
            "a discard completes one field, and the body still has to end"
        );
    }
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Clean
    );
}

/// Dropping an incomplete field cannot advance successfully to another.
async fn dropping_an_incomplete_field_poisons_the_stream(body: &[u8], row: MultipartLimits) {
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 24)
        .start();
    {
        let stream = session.stream().expect("the session holds its handle");
        let field = stream
            .next_field()
            .await
            .expect("the field parses")
            .expect("the body has a field");
        drop(field);
        assert!(
            stream.next_field().await.is_err(),
            "an abandoned field poisons the stream"
        );
    }
    assert_eq!(
        finish(session).await.terminal(),
        MultipartTerminalKind::Abandoned
    );
}

/// Revocation closes command admission from outside the handle.
async fn revocation_closes_admission_from_outside_the_handle(body: &[u8], row: MultipartLimits) {
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(body, 24)
        .start();
    session.revoke();
    let stream = session.stream().expect("the session holds its handle");
    assert!(
        matches!(stream.next_field().await, Err(RuntimeError::Multipart(_))),
        "a revoked session answers every operation without polling the body"
    );
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Abandoned);
    assert_eq!(outcome.observed().body_frames_polled(), 0);
}

#[tokio::test]
async fn nested_multipart_is_rejected_without_filesystem_capability() {
    let spellings = [
        "multipart/mixed",
        "MULTIPART/FORM-DATA",
        "Multipart/Alternative",
        "multipart/related; boundary=inner",
        "multipart/byteranges",
    ];
    let row = limits(4, 1 << 20, 4, 256, 32);

    for spelling in spellings {
        let headers =
            format!("Content-Disposition: form-data; name=\"nested\"\r\nContent-Type: {spelling}");
        let body = build(
            BOUNDARY,
            &[Part {
                headers: &headers,
                data: b"--inner\r\nContent-Disposition: form-data; name=\"deep\"\r\n\r\nx\r\n--inner--",
            }],
            b"",
        );
        let mut session = mock::multipart_session(BOUNDARY, row)
            .frames_of(&body, 21)
            .start();
        let reading = read_all(&mut session).await;
        assert!(
            matches!(reading.error, Some(RuntimeError::Multipart(_))),
            "`{spelling}` must be refused as multipart, got {:?}",
            reading.error
        );
        assert!(
            reading.fields.is_empty() && reading.chunks.is_empty(),
            "`{spelling}` must be refused before any nested field data is yielded"
        );
        let outcome = finish(session).await;
        assert_eq!(outcome.terminal(), MultipartTerminalKind::Structural);
        assert!(
            outcome
                .diagnostic()
                .is_some_and(|text| text.contains("nested")),
            "the operator diagnostic names the rule that fired: {:?}",
            outcome.diagnostic()
        );
    }

    // A part whose content type merely starts with the word is ordinary data.
    let body = build(
        BOUNDARY,
        &[Part {
            headers: "Content-Disposition: form-data; name=\"ok\"\r\nContent-Type: multipartial/plain",
            data: b"payload",
        }],
        b"",
    );
    let mut session = mock::multipart_session(BOUNDARY, row)
        .frames_of(&body, 17)
        .start();
    let reading = read_all(&mut session).await;
    assert!(reading.error.is_none(), "only `multipart/*` nests");
    assert_eq!(reading.names(), vec!["ok"]);
    let outcome = finish(session).await;
    assert_eq!(outcome.terminal(), MultipartTerminalKind::Clean);

    // Every value this module handled lived in memory: the session owns frames,
    // a parser buffer, and copied chunks, and each frame's backing is released
    // when it is spent. Nothing here names, opens, or removes a filesystem
    // object, and the module's capability proof is the final-tree Pedant scan
    // this case is paired with.
    assert_eq!(
        outcome.observed().source_frame_backings_freed(),
        Some(body.len().div_ceil(17)),
        "every controlled source frame released its in-memory backing"
    );
}

/// The checked-in seed every generated streaming multipart case derives from.
const STREAMING_PROPERTY_SEED: u64 = 0x4d50_5354_5245_0b11;

/// Generated cases one run reads, at the plan's cap for parser families.
const GENERATED_STREAMING_CASES: u64 = 128;

/// The most one generated body may carry.
const MAX_GENERATED_BODY_BYTES: usize = 64 * 1024;

/// The bytes that end one part's header block, charged to its header bound.
const HEADER_TERMINATOR_BYTES: usize = 4;

/// How many index slots one rotation of case shapes spans.
const SHAPE_SLOTS: u64 = 16;

/// Boundaries the generator frames bodies with: short, single-byte, long,
/// every punctuation byte the grammar admits, an inner space, and the
/// protocol maximum of 70 bytes.
const GENERATED_BOUNDARIES: [&str; 6] = [
    "Bnd9",
    "x",
    "----WebKitFormBoundary7MA4YWxkTrZu0gW",
    "a'()+_,-./:=?z",
    "in ner",
    "0123456789012345678901234567890123456789012345678901234567890123456789",
];

/// One parameter value as the wire spells it, and the value quoting decodes
/// it to.
///
/// The decoded column is written by hand from the quoted-string rules, never
/// computed by Camber's parser.
#[derive(Debug, Eq, PartialEq)]
struct Spelling {
    label: &'static str,
    wire: &'static str,
    decoded: &'static str,
}

const PARAMETER_SPELLINGS: [Spelling; 7] = [
    Spelling {
        label: "unquoted-token",
        wire: "field_1",
        decoded: "field_1",
    },
    Spelling {
        label: "quoted-space",
        wire: "\"plain name\"",
        decoded: "plain name",
    },
    Spelling {
        label: "quoted-separators",
        wire: "\"a;b=c\"",
        decoded: "a;b=c",
    },
    Spelling {
        label: "escaped-quote",
        wire: "\"say \\\"hi\\\"\"",
        decoded: "say \"hi\"",
    },
    Spelling {
        label: "escaped-backslash",
        wire: "\"dir\\\\file\"",
        decoded: "dir\\file",
    },
    Spelling {
        label: "quoted-pair",
        wire: "\"\\a\\b\"",
        decoded: "ab",
    },
    Spelling {
        label: "non-ascii",
        wire: "\"caf\u{e9}\"",
        decoded: "caf\u{e9}",
    },
];

/// Disposition parameters the grammar checks and then ignores.
const IGNORED_PARAMETERS: [&str; 3] = ["x-token=1", "note=\"q;v=w\"", "filename*=UTF-8''ignored"];

/// Representations a part may declare that do not nest multipart.
const CONTENT_TYPES: [(&str, &str); 4] = [
    ("text-type", "text/plain; charset=utf-8"),
    ("octet-type", "application/octet-stream"),
    ("multipart-lookalike-type", "multipartial/plain"),
    ("multipart-suffix-type", "application/x-multipart"),
];

/// Representations that nest multipart, in the spellings case folding admits.
const NESTED_CONTENT_TYPES: [&str; 4] = [
    "multipart/mixed; boundary=inner",
    "MULTIPART/FORM-DATA",
    "Multipart/Related",
    "multipart/",
];

/// How much of the boundary one delimiter lookalike repeats.
#[derive(Clone, Copy)]
enum Echo {
    Whole,
    DropLast,
    Omit,
}

/// A payload run that looks like framing and is not.
///
/// A `\r\n--` run is framing only when the whole boundary follows it and then
/// `\r\n` or `--`; each row breaks exactly one of those conditions.
struct Lookalike {
    label: &'static str,
    before: &'static [u8],
    echo: Echo,
    after: &'static [u8],
}

impl Lookalike {
    fn write(&self, boundary: &str, data: &mut Vec<u8>) {
        data.extend_from_slice(self.before);
        let echoed = match self.echo {
            Echo::Whole => boundary,
            Echo::DropLast => &boundary[..boundary.len() - 1],
            Echo::Omit => "",
        };
        data.extend_from_slice(echoed.as_bytes());
        data.extend_from_slice(self.after);
    }
}

const DELIMITER_LOOKALIKES: [Lookalike; 6] = [
    Lookalike {
        label: "lookalike-wrong-suffix",
        before: b"\r\n--",
        echo: Echo::Whole,
        after: b"x",
    },
    Lookalike {
        label: "lookalike-single-dash-suffix",
        before: b"\r\n--",
        echo: Echo::Whole,
        after: b"-x",
    },
    Lookalike {
        label: "lookalike-short-boundary",
        before: b"\r\n--",
        echo: Echo::DropLast,
        after: b"\r\n",
    },
    Lookalike {
        label: "lookalike-single-dash",
        before: b"\r\n-",
        echo: Echo::Whole,
        after: b"\r\n",
    },
    Lookalike {
        label: "lookalike-no-leading-crlf",
        before: b"z--",
        echo: Echo::Whole,
        after: b"\r\n",
    },
    Lookalike {
        label: "lookalike-binary",
        before: b"\x00\xff\r\n--",
        echo: Echo::Omit,
        after: b"\xfe",
    },
];

/// Header blocks the grammar refuses, one broken rule each.
const MALFORMED_HEADERS: [(&str, &str); 15] = [
    (
        "unterminated-quote",
        "Content-Disposition: form-data; name=\"open",
    ),
    (
        "escaped-closing-quote",
        "Content-Disposition: form-data; name=\"open\\\"",
    ),
    (
        "control-in-quotes",
        "Content-Disposition: form-data; name=\"a\u{1}b\"",
    ),
    (
        "space-in-token",
        "Content-Disposition: form-data; name=two words",
    ),
    ("empty-parameter", "Content-Disposition: form-data;; name=a"),
    (
        "parameter-without-value",
        "Content-Disposition: form-data; name",
    ),
    ("empty-name", "Content-Disposition: form-data; name=\"\""),
    (
        "repeated-name",
        "Content-Disposition: form-data; name=a; NAME=b",
    ),
    (
        "missing-name",
        "Content-Disposition: form-data; filename=f.txt",
    ),
    ("not-form-data", "Content-Disposition: attachment; name=a"),
    (
        "empty-filename",
        "Content-Disposition: form-data; name=a; filename=\"\"",
    ),
    (
        "line-without-colon",
        "Content-Disposition: form-data; name=a\r\nX-Broken",
    ),
    (
        "repeated-content-type",
        "Content-Disposition: form-data; name=a\r\nContent-Type: text/plain\r\ncontent-type: text/html",
    ),
    (
        "repeated-disposition",
        "Content-Disposition: form-data; name=a\r\nContent-Disposition: form-data; name=b",
    ),
    ("no-disposition", "X-Only: value"),
];

/// Endings after the closing delimiter the grammar refuses.
const REFUSED_EPILOGUES: [(&str, &[u8]); 5] = [
    ("epilogue-blank-line", b"\r\n\r\n"),
    ("epilogue-text", b"trailing"),
    ("epilogue-dashes", b"--"),
    ("epilogue-crlf-dash", b"\r\n-"),
    ("epilogue-partial-crlf", b"\r"),
];

/// Endings after the closing delimiter the grammar accepts.
const ACCEPTED_EPILOGUES: [(&str, &[u8]); 2] = [("no-epilogue", b""), ("crlf-epilogue", b"\r\n")];

const CHUNK_SIZES: [usize; 5] = [1, 5, 17, 64, 256];
const FRAME_SIZES: [usize; 4] = [1, 7, 64, usize::MAX];

const LOOKALIKE_RUNS: NonZeroUsize = NonZeroUsize::new(4).unwrap();
const FILLER_BYTES: NonZeroUsize = NonZeroUsize::new(129).unwrap();
/// One part in eight carries no data at all, unless its case needs bytes.
const EMPTY_DATA_ONE_IN: NonZeroUsize = NonZeroUsize::new(8).unwrap();
const FIELD_COUNT: NonZeroUsize = NonZeroUsize::new(4).unwrap();

/// Which family of rows one generated case belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Shape {
    Accepted,
    Epilogue,
    Truncated,
    Opening,
    MalformedHeader,
    Nested,
    StructuralLimit,
    ByteLimit,
}

impl Shape {
    /// The shape one index lands on, and how many cases of that shape came
    /// before it.
    ///
    /// The ordinal rotates each shape through its own variant table, so every
    /// variant is reached within the case cap rather than left to chance.
    fn of(index: u64) -> (Self, usize) {
        let (shape, first, slots) = match index % SHAPE_SLOTS {
            0..=5 => (Self::Accepted, 0, 6),
            6 => (Self::Epilogue, 6, 1),
            7 => (Self::Truncated, 7, 1),
            8 => (Self::Opening, 8, 1),
            9..=11 => (Self::MalformedHeader, 9, 3),
            12 => (Self::Nested, 12, 1),
            13 => (Self::StructuralLimit, 13, 1),
            _ => (Self::ByteLimit, 14, 2),
        };
        let ordinal = (index / SHAPE_SLOTS) * slots + (index % SHAPE_SLOTS - first);
        (shape, ordinal as usize)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Epilogue => "refused-epilogue",
            Self::Truncated => "truncated",
            Self::Opening => "refused-opening",
            Self::MalformedHeader => "malformed-header",
            Self::Nested => "nested-part",
            Self::StructuralLimit => "structural-limit",
            Self::ByteLimit => "byte-limit",
        }
    }

    /// What the parts of this shape must carry for its refusal to exist.
    fn demand(self, ordinal: usize) -> Demand {
        match (self, ordinal % 3) {
            (Self::StructuralLimit, 0) => Demand::with_fields(2),
            (Self::StructuralLimit, 1) => Demand::with_extra_header(),
            (Self::ByteLimit, _) => Demand::with_data(),
            _ => Demand::with_fields(1),
        }
    }
}

/// The least a case's parts must carry.
#[derive(Clone, Copy)]
struct Demand {
    min_fields: usize,
    extra_header: bool,
    min_data: usize,
}

impl Demand {
    fn with_fields(min_fields: usize) -> Self {
        Self {
            min_fields,
            extra_header: false,
            min_data: 0,
        }
    }

    fn with_extra_header() -> Self {
        Self {
            extra_header: true,
            ..Self::with_fields(1)
        }
    }

    fn with_data() -> Self {
        Self {
            min_data: 2,
            ..Self::with_fields(1)
        }
    }
}

/// One generated part: its exact header block and what it must read as.
#[derive(Debug, Eq, PartialEq)]
struct GeneratedPart {
    headers: Box<str>,
    expected: Collected,
}

impl GeneratedPart {
    fn lines(&self) -> usize {
        self.headers.split("\r\n").count()
    }

    fn header_bytes(&self) -> usize {
        self.headers.len() + HEADER_TERMINATOR_BYTES
    }

    fn data_bytes(&self) -> usize {
        self.expected.data.len()
    }
}

/// How one session's body is cut into source frames: everything before `edge`
/// as one frame, then the rest in frames of at most `size` bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Framing {
    edge: usize,
    size: usize,
}

/// How one generated session must end.
#[derive(Debug, Eq, PartialEq)]
enum Ending {
    Clean,
    /// `complete` leading fields arrive whole. An `open` refusal may also
    /// deliver the next field's metadata and a prefix of its data.
    Refused {
        terminal: MultipartTerminalKind,
        complete: usize,
        open: bool,
        diagnostic: Option<&'static str>,
    },
}

impl Ending {
    fn structural(complete: usize) -> Self {
        Self::Refused {
            terminal: MultipartTerminalKind::Structural,
            complete,
            open: false,
            diagnostic: None,
        }
    }

    fn open(terminal: MultipartTerminalKind, complete: usize) -> Self {
        Self::Refused {
            terminal,
            complete,
            open: true,
            diagnostic: None,
        }
    }
}

/// One reproducible generated session family.
#[derive(Debug, Eq, PartialEq)]
struct StreamingCase {
    labels: Box<[&'static str]>,
    boundary: &'static str,
    body: Box<[u8]>,
    limits: MultipartLimits,
    body_limit: usize,
    framings: Box<[Framing]>,
    expected: Box<[Collected]>,
    ending: Ending,
}

/// Where each part sits in a body framed by the shared fixture.
///
/// Computed from lengths alone and then checked against the framed bytes, so a
/// layout that disagreed with the framing fails the case instead of cutting it
/// somewhere else.
struct Layout {
    header_starts: Box<[usize]>,
    data_starts: Box<[usize]>,
    data_ends: Box<[usize]>,
    delimiter: usize,
}

impl Layout {
    fn of(boundary: &str, parts: &[GeneratedPart]) -> Self {
        let mut at = opening_bytes(boundary);
        let delimiter = 2 + at;
        let mut header_starts = Vec::with_capacity(parts.len());
        let mut data_starts = Vec::with_capacity(parts.len());
        let mut data_ends = Vec::with_capacity(parts.len());
        for part in parts {
            header_starts.push(at);
            let data_start = at + part.header_bytes();
            data_starts.push(data_start);
            let data_end = data_start + part.data_bytes();
            data_ends.push(data_end);
            at = data_end + delimiter;
        }
        Self {
            header_starts: header_starts.into_boxed_slice(),
            data_starts: data_starts.into_boxed_slice(),
            data_ends: data_ends.into_boxed_slice(),
            delimiter,
        }
    }

    /// The layout of one framed body, checked against its bytes.
    fn checked(boundary: &str, parts: &[GeneratedPart], body: &[u8]) -> Self {
        let layout = Self::of(boundary, parts);
        for (index, part) in parts.iter().enumerate() {
            assert_eq!(
                &body[layout.data_starts[index]..layout.data_ends[index]],
                part.expected.data.as_slice(),
                "the layout locates part {index}'s data"
            );
        }
        layout
    }

    /// Where the opening delimiter line ends.
    fn opening(&self) -> usize {
        *self
            .header_starts
            .first()
            .expect("a generated body has a part")
    }

    fn closing(&self) -> usize {
        *self.data_ends.last().expect("a generated body has a part")
    }
}

/// Every part the generator frames obeys the sender's rule: the boundary's
/// delimiter first occurs where the part's data ends.
///
/// Stated from the multipart framing rule, not read from Camber's search: a
/// payload that already contained its own delimiter would be a different body.
fn assert_delimiter_free(data: &[u8], boundary: &str) {
    let prefix = [data, b"\r\n--", boundary.as_bytes()].concat();
    let mut framed = Vec::with_capacity(prefix.len() + 2);
    for suffix in [b"\r\n".as_slice(), b"--"] {
        framed.clear();
        framed.extend_from_slice(&prefix);
        framed.extend_from_slice(suffix);
        let delimiter = &framed[data.len()..];
        let first = framed
            .windows(delimiter.len())
            .position(|window| window == delimiter);
        assert_eq!(
            first,
            Some(data.len()),
            "a generated payload never carries its own delimiter"
        );
    }
}

/// One part's payload: delimiter lookalikes, filler, and possibly a partial
/// delimiter as its final bytes.
fn generated_data(
    case: &mut DeterministicCase,
    boundary: &str,
    min_data: usize,
    labels: &mut Vec<&'static str>,
) -> Vec<u8> {
    let mut data = Vec::new();
    if min_data == 0 && case.bounded(EMPTY_DATA_ONE_IN) == 0 {
        labels.push("empty-data");
        return data;
    }
    for run in 0..case.bounded(LOOKALIKE_RUNS) {
        let lookalike = case.pick(&DELIMITER_LOOKALIKES);
        labels.push(lookalike.label);
        lookalike.write(boundary, &mut data);
        write!(data, "|{}.{run}", case.index()).expect("a Vec accepts every write");
    }
    let filler = case
        .bounded(FILLER_BYTES)
        .max(min_data.saturating_sub(data.len()));
    data.extend((0..filler).map(|offset| b'a' + (offset % 26) as u8));
    if case.boolean() {
        labels.push("partial-delimiter-at-end");
        data.extend_from_slice(b"\r\n--");
        data.extend_from_slice(&boundary.as_bytes()[..boundary.len() / 2]);
    }
    assert_delimiter_free(&data, boundary);
    data
}

/// One `Content-Disposition` value in a generated spelling.
fn disposition_value(
    case: &mut DeterministicCase,
    name: &Spelling,
    filename: Option<&Spelling>,
    labels: &mut Vec<&'static str>,
) -> String {
    let token = case.pick(&["form-data", "FORM-DATA", "Form-Data"]);
    let separator = case.pick(&["; ", ";", " ;  "]);
    let equals = case.pick(&["=", " = "]);
    if separator.len() != 2 || equals.len() != 1 {
        labels.push("parameter-whitespace");
    }
    let name_key = case.pick(&["name", "NAME", "Name"]);
    let mut parameters = vec![format!("{name_key}{equals}{}", name.wire)];
    if let Some(filename) = filename {
        let key = case.pick(&["filename", "FILENAME", "FileName"]);
        let parameter = format!("{key}{equals}{}", filename.wire);
        labels.push(place_filename(case, &mut parameters, parameter));
    }
    if case.boolean() {
        labels.push("ignored-parameter");
        let ignored = *case.pick(&IGNORED_PARAMETERS);
        parameters.push(ignored.to_owned());
    }
    format!("{token}{separator}{}", parameters.join(separator))
}

/// Place one filename parameter before or after the name, and label where.
fn place_filename(
    case: &mut DeterministicCase,
    parameters: &mut Vec<String>,
    parameter: String,
) -> &'static str {
    match case.boolean() {
        true => {
            parameters.insert(0, parameter);
            "filename-first"
        }
        false => {
            parameters.push(parameter);
            "filename"
        }
    }
}

/// One part with a generated header block and payload.
fn generated_part(
    case: &mut DeterministicCase,
    boundary: &str,
    demand: Demand,
    labels: &mut Vec<&'static str>,
) -> GeneratedPart {
    let name = case.pick(&PARAMETER_SPELLINGS);
    labels.push(name.label);
    let filename = case.boolean().then(|| case.pick(&PARAMETER_SPELLINGS));
    let content_type = case.boolean().then(|| *case.pick(&CONTENT_TYPES));
    let disposition = disposition_value(case, name, filename, labels);
    let disposition_header = case.pick(&[
        "Content-Disposition",
        "content-disposition",
        "CONTENT-DISPOSITION",
    ]);
    let mut lines = vec![format!("{disposition_header}: {disposition}")];
    if let Some((label, value)) = content_type {
        labels.push(label);
        let header = case.pick(&["Content-Type", "content-type", "CONTENT-TYPE"]);
        lines.push(format!("{header}: {value}"));
    }
    if demand.extra_header || case.boolean() {
        labels.push("extra-header");
        let at = case.bounded(NonZeroUsize::MIN.saturating_add(lines.len()));
        lines.insert(at, "X-Extra: kept".to_owned());
    }
    let data = generated_data(case, boundary, demand.min_data, labels);
    GeneratedPart {
        headers: lines.join("\r\n").into_boxed_str(),
        expected: Collected {
            name: name.decoded.into(),
            filename: filename.map(|spelling| spelling.decoded.into()),
            content_type: content_type.map(|(_, value)| value.into()),
            data,
        },
    }
}

/// The parts one case frames, in wire order.
fn generated_parts(
    case: &mut DeterministicCase,
    boundary: &str,
    demand: Demand,
    labels: &mut Vec<&'static str>,
) -> Box<[GeneratedPart]> {
    let count = (1 + case.bounded(FIELD_COUNT)).max(demand.min_fields);
    let parts: Box<[GeneratedPart]> = (0..count)
        .map(|_| generated_part(case, boundary, demand, labels))
        .collect();
    let mut names: Vec<&str> = parts.iter().map(|part| &*part.expected.name).collect();
    names.sort_unstable();
    if names.windows(2).any(|pair| pair[0] == pair[1]) {
        labels.push("duplicate-name");
    }
    parts
}

/// The bounds every part fits exactly: each maximum equals what the largest
/// part needs.
fn tight_bounds(parts: &[GeneratedPart], chunk: usize) -> Bounds {
    let largest = |measure: fn(&GeneratedPart) -> usize| first_largest(parts, measure).1.max(1);
    Bounds {
        fields: parts.len(),
        field_bytes: largest(GeneratedPart::data_bytes),
        headers: largest(GeneratedPart::lines),
        header_bytes: largest(GeneratedPart::header_bytes),
        chunk,
    }
}

/// The first part whose measure is the largest, and that measure.
fn first_largest(parts: &[GeneratedPart], measure: fn(&GeneratedPart) -> usize) -> (usize, usize) {
    let largest = parts.iter().map(measure).max().expect("a case has parts");
    let index = parts
        .iter()
        .position(|part| measure(part) == largest)
        .expect("the largest measure belongs to a part");
    (index, largest)
}

/// Frame the generated parts under the shared fixture's framing.
fn frame_parts(boundary: &str, parts: &[GeneratedPart], epilogue: &[u8]) -> Vec<u8> {
    frame_raw(
        boundary,
        parts
            .iter()
            .map(|part| (&*part.headers, part.expected.data.as_slice())),
        epilogue,
    )
}

/// Everything a shape needs to turn parts into one case.
struct Draft {
    labels: Vec<&'static str>,
    boundary: &'static str,
    parts: Box<[GeneratedPart]>,
    chunk: usize,
    frame: usize,
    ordinal: usize,
}

impl Draft {
    /// Seal a draft into its case, reading the parts as its expectations.
    fn seal(
        self,
        body: Vec<u8>,
        bounds: Bounds,
        body_limit: usize,
        framings: Box<[Framing]>,
        ending: Ending,
    ) -> StreamingCase {
        StreamingCase {
            labels: self.labels.into_boxed_slice(),
            boundary: self.boundary,
            body: body.into_boxed_slice(),
            limits: bounds.within(self.boundary),
            body_limit,
            framings,
            expected: self.parts.into_iter().map(|part| part.expected).collect(),
            ending,
        }
    }

    /// Seal one refusal read as a single uniformly framed session.
    fn refuse(self, body: Vec<u8>, bounds: Bounds, ending: Ending) -> StreamingCase {
        let framing = Framing {
            edge: 0,
            size: self.frame,
        };
        let body_limit = body.len();
        self.seal(body, bounds, body_limit, Box::new([framing]), ending)
    }

    /// Seal one whole body read cleanly under tight bounds, once per framing.
    fn accept(self, body: Vec<u8>, framings: Box<[Framing]>) -> StreamingCase {
        let bounds = self.tight();
        let body_limit = body.len();
        self.seal(body, bounds, body_limit, framings, Ending::Clean)
    }

    fn tight(&self) -> Bounds {
        tight_bounds(&self.parts, self.chunk)
    }

    /// The draft's parts framed under its boundary.
    fn framed(&self, epilogue: &[u8]) -> Vec<u8> {
        frame_parts(self.boundary, &self.parts, epilogue)
    }

    /// Where each part sits in `body`, checked against its bytes.
    fn layout(&self, body: &[u8]) -> Layout {
        Layout::checked(self.boundary, &self.parts, body)
    }

    /// One part index, drawn uniformly.
    fn any_part(&self, case: &mut DeterministicCase) -> usize {
        case.below(self.parts.len())
    }

    /// Replace one part's header block, so the refusal it carries is the
    /// case's own.
    fn replace_headers(&mut self, target: usize, headers: &str) {
        self.parts[target].headers = headers.into();
    }
}

/// A body read cleanly under exactly tight limits, with a frame edge at every
/// offset of one selected delimiter.
fn accepted_case(mut draft: Draft, case: &mut DeterministicCase) -> StreamingCase {
    let (label, epilogue) = *case.pick(&ACCEPTED_EPILOGUES);
    draft.labels.push(label);
    let body = draft.framed(epilogue);
    let layout = draft.layout(&body);
    let site = case.bounded(NonZeroUsize::MIN.saturating_add(draft.parts.len()));
    let (label, start, end) = match site {
        0 => ("split-opening", 0, layout.opening()),
        site if site == draft.parts.len() => ("split-closing", layout.closing(), body.len()),
        site => {
            let start = layout.data_ends[site - 1];
            ("split-separator", start, start + layout.delimiter)
        }
    };
    draft.labels.push(label);
    let framings = (start..=end)
        .map(|edge| Framing {
            edge,
            size: draft.frame,
        })
        .collect();
    draft.accept(body, framings)
}

/// Every field arrives, and the ending after the closing delimiter is refused.
fn epilogue_case(mut draft: Draft) -> StreamingCase {
    let (label, epilogue) = REFUSED_EPILOGUES[draft.ordinal % REFUSED_EPILOGUES.len()];
    draft.labels.push(label);
    let body = draft.framed(epilogue);
    let (bounds, complete) = (draft.tight(), draft.parts.len());
    draft.refuse(body, bounds, Ending::structural(complete))
}

/// The body stops inside the closing delimiter or inside the last header.
fn truncated_case(mut draft: Draft, case: &mut DeterministicCase) -> StreamingCase {
    let mut body = draft.framed(b"");
    let layout = draft.layout(&body);
    let last = draft.parts.len() - 1;
    let (label, cut, ending) = match draft.ordinal % 2 {
        0 => {
            let cut = layout.closing() + case.below(layout.delimiter);
            let ending = Ending::open(MultipartTerminalKind::Structural, last);
            ("truncated-in-closing-delimiter", cut, ending)
        }
        _ => {
            let within = draft.parts[last].header_bytes();
            let cut = layout.header_starts[last] + case.below(within);
            ("truncated-in-last-header", cut, Ending::structural(last))
        }
    };
    draft.labels.push(label);
    body.truncate(cut);
    let bounds = draft.tight();
    draft.refuse(body, bounds, ending)
}

/// The body does not open with this boundary's delimiter line.
fn opening_case(mut draft: Draft) -> StreamingCase {
    let framed = draft.framed(b"");
    let suffix_at = 2 + draft.boundary.len();
    let (label, body) = match draft.ordinal % 3 {
        0 => (
            "opening-preamble",
            [b"preamble\r\n".as_slice(), &framed].concat(),
        ),
        1 => {
            let mut body = framed;
            body[suffix_at..suffix_at + 2].copy_from_slice(b"x-");
            ("opening-suffix", body)
        }
        _ => (
            "opening-other-boundary",
            frame_parts("Other7", &draft.parts, b""),
        ),
    };
    draft.labels.push(label);
    let bounds = draft.tight();
    draft.refuse(body, bounds, Ending::structural(0))
}

/// One part's header block breaks one grammar rule.
fn malformed_header_case(mut draft: Draft, case: &mut DeterministicCase) -> StreamingCase {
    let (label, headers) = MALFORMED_HEADERS[draft.ordinal % MALFORMED_HEADERS.len()];
    draft.labels.push(label);
    let target = draft.any_part(case);
    draft.replace_headers(target, headers);
    let body = draft.framed(b"");
    let bounds = draft.tight();
    draft.refuse(body, bounds, Ending::structural(target))
}

/// One part declares a nested multipart representation.
fn nested_case(mut draft: Draft, case: &mut DeterministicCase) -> StreamingCase {
    let spelling = NESTED_CONTENT_TYPES[draft.ordinal % NESTED_CONTENT_TYPES.len()];
    draft.labels.push("nested");
    let target = draft.any_part(case);
    let headers =
        format!("Content-Disposition: form-data; name=\"nested\"\r\nContent-Type: {spelling}");
    draft.replace_headers(target, &headers);
    let body = draft.framed(b"");
    let bounds = draft.tight();
    let ending = Ending::Refused {
        terminal: MultipartTerminalKind::Structural,
        complete: target,
        open: false,
        diagnostic: Some("nested"),
    };
    draft.refuse(body, bounds, ending)
}

/// One structural bound sits one below what the body needs.
fn structural_limit_case(mut draft: Draft) -> StreamingCase {
    let body = draft.framed(b"");
    let tight = draft.tight();
    let (label, bounds, complete) = match draft.ordinal % 3 {
        0 => {
            let fields = draft.parts.len() - 1;
            ("fields-over-bound", Bounds { fields, ..tight }, fields)
        }
        1 => {
            let (target, lines) = first_largest(&draft.parts, GeneratedPart::lines);
            let headers = lines - 1;
            ("headers-over-bound", Bounds { headers, ..tight }, target)
        }
        _ => {
            let (target, bytes) = first_largest(&draft.parts, GeneratedPart::header_bytes);
            let header_bytes = bytes - 1;
            (
                "header-bytes-over-bound",
                Bounds {
                    header_bytes,
                    ..tight
                },
                target,
            )
        }
    };
    draft.labels.push(label);
    draft.refuse(body, bounds, Ending::structural(complete))
}

/// A field's bytes, or the admitted total, cross by exactly one byte.
fn byte_limit_case(draft: Draft, case: &mut DeterministicCase) -> StreamingCase {
    match draft.ordinal % 2 {
        0 => field_bytes_case(draft),
        _ => body_bytes_case(draft, case),
    }
}

/// The largest field is one byte over its per-field bound.
fn field_bytes_case(mut draft: Draft) -> StreamingCase {
    draft.labels.push("field-bytes-over-bound");
    let body = draft.framed(b"");
    let (target, bytes) = first_largest(&draft.parts, GeneratedPart::data_bytes);
    let bounds = Bounds {
        field_bytes: bytes - 1,
        ..draft.tight()
    };
    let ending = Ending::open(MultipartTerminalKind::ByteLimit, target);
    draft.refuse(body, bounds, ending)
}

/// The admitted total ends inside one field's data, and the frame that
/// crosses it is the first frame after that field's header block.
fn body_bytes_case(mut draft: Draft, case: &mut DeterministicCase) -> StreamingCase {
    draft.labels.push("body-bytes-over-bound");
    let body = draft.framed(b"");
    let layout = draft.layout(&body);
    let target = draft.any_part(case);
    let length = NonZeroUsize::new(draft.parts[target].data_bytes())
        .expect("a byte-limit case carries data in every part");
    let edge = layout.data_starts[target];
    let body_limit = edge + case.bounded(length);
    let framings = Box::new([Framing {
        edge,
        size: draft.frame,
    }]);
    let ending = Ending::open(MultipartTerminalKind::ByteLimit, target);
    let tight = draft.tight();
    draft.seal(body, tight, body_limit, framings, ending)
}

/// Build one generated case from its seed and index alone.
fn generated_streaming_case(case: &mut DeterministicCase) -> StreamingCase {
    let (shape, ordinal) = Shape::of(case.index());
    let boundary = *case.pick(&GENERATED_BOUNDARIES);
    let mut labels = vec![shape.label()];
    let parts = generated_parts(case, boundary, shape.demand(ordinal), &mut labels);
    let chunk = *case.pick(&CHUNK_SIZES);
    let frame = *case.pick(&FRAME_SIZES);
    let draft = Draft {
        labels,
        boundary,
        parts,
        chunk,
        frame,
        ordinal,
    };
    match shape {
        Shape::Accepted => accepted_case(draft, case),
        Shape::Epilogue => epilogue_case(draft),
        Shape::Truncated => truncated_case(draft, case),
        Shape::Opening => opening_case(draft),
        Shape::MalformedHeader => malformed_header_case(draft, case),
        Shape::Nested => nested_case(draft, case),
        Shape::StructuralLimit => structural_limit_case(draft),
        Shape::ByteLimit => byte_limit_case(draft, case),
    }
}

/// Start one controlled session over a generated body cut by one framing, and
/// report how many source frames it was handed.
fn start_generated(generated: &StreamingCase, framing: Framing) -> (MultipartSession, usize) {
    let (head, tail) = generated.body.split_at(framing.edge);
    let mut builder = mock::multipart_session(generated.boundary, generated.limits)
        .body_limit(generated.body_limit)
        .with_permit();
    if !head.is_empty() {
        builder = builder.frame(head);
    }
    let frames = usize::from(!head.is_empty()) + tail.chunks(framing.size).count();
    (builder.frames_of(tail, framing.size).start(), frames)
}

/// Whether one refusal reached application code under its own provenance.
fn refused_with(terminal: MultipartTerminalKind, error: Option<&RuntimeError>) -> bool {
    matches!(
        (terminal, error),
        (
            MultipartTerminalKind::Structural,
            Some(RuntimeError::Multipart(_))
        ) | (
            MultipartTerminalKind::ByteLimit,
            Some(RuntimeError::RequestBodyLimit(_))
        )
    )
}

/// The fields before a refusal arrive whole; an open refusal may add the next
/// field's metadata with a prefix of its data, and nothing else arrives.
fn assert_refused_delivery(
    context: &str,
    expected: &[Collected],
    delivered: &[Collected],
    complete: usize,
    open: bool,
) {
    assert!(
        delivered.len() >= complete,
        "{context}: {complete} fields precede the refusal, {} arrived",
        delivered.len()
    );
    let (whole, rest) = delivered.split_at(complete);
    assert_eq!(
        whole,
        &expected[..complete],
        "{context}: fields before the refusal"
    );
    match (open, rest) {
        (_, []) => {}
        (true, [partial]) => {
            let field = &expected[complete];
            assert_eq!(
                partial.metadata(),
                field.metadata(),
                "{context}: refused field metadata"
            );
            assert!(
                field.data.starts_with(&partial.data),
                "{context}: the refused field delivered {} bytes that are not its prefix",
                partial.data.len()
            );
        }
        _ => panic!("{context}: {} fields arrived past the refusal", rest.len()),
    }
}

/// The reading, and the terminal the driver returned, match the case.
fn assert_generated_ending(
    context: &str,
    generated: &StreamingCase,
    reading: &Reading,
    outcome: &MultipartOutcome,
) {
    let chunk = generated.limits.max_chunk_bytes();
    assert!(
        reading.chunks.iter().all(|size| (1..=chunk).contains(size)),
        "{context}: every chunk carries 1..={chunk} bytes: {:?}",
        reading.chunks
    );
    match generated.ending {
        Ending::Clean => {
            assert!(
                reading.error.is_none(),
                "{context}: a valid body under validated limits reads cleanly, got {:?}",
                reading.error
            );
            assert_eq!(&*reading.fields, &*generated.expected, "{context}: fields");
            assert_eq!(
                outcome.terminal(),
                MultipartTerminalKind::Clean,
                "{context}"
            );
        }
        Ending::Refused {
            terminal,
            complete,
            open,
            diagnostic,
        } => {
            let context = format!("{context} error={:?}", reading.error);
            assert!(
                refused_with(terminal, reading.error.as_ref()),
                "{context}: must be refused as {terminal:?}"
            );
            assert_refused_delivery(
                &context,
                &generated.expected,
                &reading.fields,
                complete,
                open,
            );
            assert_eq!(outcome.terminal(), terminal, "{context}: terminal");
            let named = diagnostic
                .is_none_or(|rule| outcome.diagnostic().is_some_and(|text| text.contains(rule)));
            assert!(named, "{context}: diagnostic {:?}", outcome.diagnostic());
        }
    }
}

/// The finished session held no more than its bounds and released every frame
/// backing, its permit, and its reply exactly once.
fn assert_generated_release(
    context: &str,
    generated: &StreamingCase,
    outcome: &MultipartOutcome,
    frames: usize,
) {
    let observed = outcome.observed();
    let limits = generated.limits;
    assert!(
        observed.parser_peak_bytes() <= limits.max_parser_buffer_bytes(),
        "{context}: parser peak {} over {}",
        observed.parser_peak_bytes(),
        limits.max_parser_buffer_bytes()
    );
    assert!(
        observed.reply_peak_bytes() <= limits.max_reply_bytes(),
        "{context}: reply peak {} over {}",
        observed.reply_peak_bytes(),
        limits.max_reply_bytes()
    );
    assert!(
        observed.active_metadata_peak_bytes() <= limits.max_header_bytes_per_field(),
        "{context}: active metadata peak {} over {}",
        observed.active_metadata_peak_bytes(),
        limits.max_header_bytes_per_field()
    );
    assert_eq!(
        observed.source_frame_backings_freed(),
        Some(frames),
        "{context}: frames"
    );
    assert_eq!(
        observed.permit_owners_dropped(),
        1,
        "{context}: permit owner"
    );
    assert_eq!(
        observed.permit_backings_freed(),
        Some(1),
        "{context}: permit"
    );
    assert_eq!(observed.drivers_terminated(), 1, "{context}: driver");
    assert_eq!(observed.reply_retained_bytes(), 0, "{context}: reply");
    if generated.ending == Ending::Clean {
        assert_eq!(observed.parser_retained_bytes(), 0, "{context}: parser");
    }
}

/// Read one generated case under one framing to its terminal, require the
/// ending and release the case names, and hand back the reading.
async fn assert_generated_framing(
    context: &str,
    generated: &StreamingCase,
    framing: Framing,
) -> Reading {
    let (mut session, frames) = start_generated(generated, framing);
    let reading = read_all(&mut session).await;
    let outcome = finish(session).await;
    assert_generated_ending(context, generated, &reading, &outcome);
    assert_generated_release(context, generated, &outcome, frames);
    reading
}

/// Read one generated case under every framing it names, to its terminal.
async fn assert_generated_case(case: &DeterministicCase, generated: &StreamingCase) {
    let labels = generated.labels.join(",");
    assert!(
        generated.body.len() <= MAX_GENERATED_BODY_BYTES,
        "{case} [{labels}]: {} body bytes",
        generated.body.len()
    );
    for framing in generated.framings.iter().copied() {
        let context = format!(
            "{case} [{labels}] edge={} frame={}",
            framing.edge, framing.size
        );
        assert_generated_framing(&context, generated, framing).await;
    }
}

/// Every label a complete run must reach.
///
/// The shape labels come from the rotation itself, so a shape added to it is
/// required without a second list to keep in step.
fn required_streaming_labels() -> impl Iterator<Item = &'static str> {
    let fixed = [
        "split-opening",
        "split-separator",
        "split-closing",
        "partial-delimiter-at-end",
        "empty-data",
        "duplicate-name",
        "filename-first",
        "ignored-parameter",
        "parameter-whitespace",
        "extra-header",
        "truncated-in-closing-delimiter",
        "truncated-in-last-header",
        "opening-preamble",
        "opening-suffix",
        "opening-other-boundary",
        "fields-over-bound",
        "headers-over-bound",
        "header-bytes-over-bound",
        "field-bytes-over-bound",
        "body-bytes-over-bound",
    ];
    (0..SHAPE_SLOTS)
        .map(|index| Shape::of(index).0.label())
        .chain(fixed)
        .chain(PARAMETER_SPELLINGS.iter().map(|spelling| spelling.label))
        .chain(CONTENT_TYPES.iter().map(|(label, _)| *label))
        .chain(DELIMITER_LOOKALIKES.iter().map(|lookalike| lookalike.label))
        .chain(MALFORMED_HEADERS.iter().map(|(label, _)| *label))
        .chain(REFUSED_EPILOGUES.iter().map(|(label, _)| *label))
        .chain(ACCEPTED_EPILOGUES.iter().map(|(label, _)| *label))
}

#[tokio::test]
async fn generated_streaming_multipart_fragments_preserve_field_contract() {
    let generator = DeterministicGenerator::new(STREAMING_PROPERTY_SEED);
    let mut reached = BTreeSet::new();
    for index in 0..GENERATED_STREAMING_CASES {
        let (case, generated) = generator.reproducible(index, generated_streaming_case);
        assert_eq!(
            case.seed(),
            STREAMING_PROPERTY_SEED,
            "{case}: checked-in seed"
        );
        assert_generated_case(&case, &generated).await;
        reached.extend(generated.labels.iter().copied());
    }
    generator.assert_reached(
        GENERATED_STREAMING_CASES,
        required_streaming_labels(),
        &reached,
    );
}

/// The chunk bound the reduced regression reads under: above its header bound.
const OVERLAP_CHUNK_BYTES: usize = 256;

/// The two parts reduced from `seed=0x4d50535452450b11 case=6`.
///
/// The second header block is the larger, so it sets the header bound, and its
/// data outruns one chunk so a data window can fill past the first delimiter.
fn overlap_parts() -> Box<[GeneratedPart]> {
    let part = |headers: String, name: &str, data: Vec<u8>| GeneratedPart {
        headers: headers.into_boxed_str(),
        expected: Collected {
            name: name.into(),
            filename: None,
            content_type: None,
            data,
        },
    };
    Box::new([
        part(
            "Content-Disposition: form-data; name=\"first\"".to_owned(),
            "first",
            b"one".to_vec(),
        ),
        part(
            format!(
                "Content-Disposition: form-data; name=\"second\"\r\nX-Padding: {}",
                "p".repeat(40)
            ),
            "second",
            vec![b'd'; 200],
        ),
    ])
}

/// The largest header block the overlap body carries.
fn overlap_header_bytes() -> usize {
    first_largest(&overlap_parts(), GeneratedPart::header_bytes).1
}

/// The overlap body under tight bounds at one chunk limit, expected clean.
///
/// Two framings: the whole body as one source frame, and the same bytes cut
/// where the second header block begins. The cut frame hands the header phase
/// an empty buffer; the whole frame hands it whatever the data window filled.
fn overlap_case(chunk: usize) -> StreamingCase {
    let draft = Draft {
        labels: vec!["overlap-regression"],
        boundary: BOUNDARY,
        parts: overlap_parts(),
        chunk,
        frame: usize::MAX,
        ordinal: 0,
    };
    let body = draft.framed(b"");
    let layout = draft.layout(&body);
    let framings = Box::new([
        Framing {
            edge: 0,
            size: usize::MAX,
        },
        Framing {
            edge: layout.header_starts[1],
            size: usize::MAX,
        },
    ]);
    draft.accept(body, framings)
}

/// Read the overlap body under one framing, finish the session, then require a
/// clean read of both exact fields within the validated buffer and a complete
/// release.
async fn assert_overlap_reads_cleanly(generated: &StreamingCase, framing: Framing) -> Reading {
    let limits = generated.limits;
    let context = format!(
        "overlap chunk={} header={} buffer={} edge={}",
        limits.max_chunk_bytes(),
        limits.max_header_bytes_per_field(),
        limits.max_parser_buffer_bytes(),
        framing.edge
    );
    assert_eq!(
        generated.ending,
        Ending::Clean,
        "{context}: the overlap body"
    );
    assert_generated_framing(&context, generated, framing).await
}

/// Both framings of the overlap body at one chunk limit read the same fields.
async fn assert_framings_agree(chunk: usize) {
    let generated = overlap_case(chunk);
    let whole = assert_overlap_reads_cleanly(&generated, generated.framings[0]).await;
    let split = assert_overlap_reads_cleanly(&generated, generated.framings[1]).await;
    assert_eq!(
        whole.fields, split.fields,
        "chunk={chunk}: whole-frame and split-frame delivery read the same fields"
    );
}

/// Reduced from `seed=0x4d50535452450b11 case=6`, read as one source frame.
///
/// A valid two-field body under limits the builder validated: the chunk bound
/// exceeds the header bound, and one source frame carries the first field, its
/// delimiter, the whole second header block, and more data. The data phase
/// fills to one chunk plus delimiter carry, so the second header block and the
/// data after it are already retained when that block's metadata is reserved.
#[tokio::test]
async fn header_metadata_beside_a_filled_data_window_fits_the_validated_buffer() {
    let generated = overlap_case(OVERLAP_CHUNK_BYTES);
    assert!(
        generated.limits.max_chunk_bytes() > generated.limits.max_header_bytes_per_field(),
        "this case configures chunk bytes above header bytes"
    );
    let reading = assert_overlap_reads_cleanly(&generated, generated.framings[0]).await;
    assert_eq!(reading.names(), vec!["first", "second"]);
}

/// The same body and limits, cut where the second header block begins: the
/// header phase starts from an empty buffer, so no data window overlaps it.
#[tokio::test]
async fn overlap_body_split_before_the_second_header_block_reads_cleanly() {
    let generated = overlap_case(OVERLAP_CHUNK_BYTES);
    let reading = assert_overlap_reads_cleanly(&generated, generated.framings[1]).await;
    assert_eq!(reading.names(), vec!["first", "second"]);
}

/// A chunk bound below the header bound leaves a filled data window smaller
/// than one header block, in either framing.
#[tokio::test]
async fn chunk_limit_below_the_header_limit_reads_the_overlap_body_in_either_framing() {
    assert_framings_agree(overlap_header_bytes() - 1).await;
}

/// A chunk bound equal to the header bound, in either framing.
#[tokio::test]
async fn chunk_limit_equal_to_the_header_limit_reads_the_overlap_body_in_either_framing() {
    assert_framings_agree(overlap_header_bytes()).await;
}
