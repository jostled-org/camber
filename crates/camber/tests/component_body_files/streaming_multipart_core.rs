//! The exact incremental parser and session contract, driven through the
//! production driver by a controlled body.
//!
//! Every case here enters the same generic session code a served route will
//! instantiate. The harness supplies frames and scheduling; it chooses no parser
//! state, no budget, no terminal summary, and no refusal.

use crate::streaming_multipart as fixture;

use camber::RuntimeError;
use camber::http::mock::{
    self, MultipartObservation, MultipartOutcome, MultipartSession, MultipartTerminalKind,
};
use camber::http::{MultipartField, MultipartLimits, MultipartStream};
use std::future::Future;
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
    let raw: Vec<(&str, &[u8])> = parts.iter().map(|part| (part.headers, part.data)).collect();
    fixture::raw_multipart_body(boundary, &raw, epilogue)
}

/// One row's limits, with the parser buffer at exactly the peak they require.
///
/// Tight rather than generous: a buffer with slack would hide an accounting
/// error the bound is supposed to catch.
fn limits(
    fields: usize,
    field_bytes: usize,
    headers: usize,
    header_bytes: usize,
    chunk: usize,
) -> MultipartLimits {
    let required = (2 * header_bytes).max(chunk + BOUNDARY.len() + 5);
    MultipartLimits::builder()
        .max_fields(fields)
        .max_field_bytes(field_bytes)
        .max_headers_per_field(headers)
        .max_header_bytes_per_field(header_bytes)
        .max_boundary_bytes(BOUNDARY.len())
        .max_chunk_bytes(chunk)
        .max_parser_buffer_bytes(required)
        .build()
        .expect("the row's limits are a valid combination")
}

/// Limits generous enough that only the row's own subject can refuse anything.
fn permissive(chunk: usize) -> MultipartLimits {
    limits(64, 1 << 20, 16, 1024, chunk)
}

/// What one field turned out to be.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Collected {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    data: Vec<u8>,
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
        self.fields
            .iter()
            .map(|field| field.name.as_str())
            .collect()
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
        name: field.name().to_owned(),
        filename: field.filename().map(str::to_owned),
        content_type: field.content_type().map(str::to_owned),
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
            name: "alpha".to_owned(),
            filename: None,
            content_type: None,
            data: b"one".to_vec(),
        },
        Collected {
            name: "file".to_owned(),
            filename: Some("a\"b .txt".to_owned()),
            content_type: Some("application/octet-stream".to_owned()),
            data: binary.to_vec(),
        },
        Collected {
            name: "empty".to_owned(),
            filename: None,
            content_type: None,
            data: Vec::new(),
        },
        Collected {
            name: "alpha".to_owned(),
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
    let opening = 2 + BOUNDARY.len() + 2;
    let head = opening + header_bytes;

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
