//! Step 9 fixtures: a held streaming source, one HTTP/1 peer that reads its
//! head before the source is released, and the completion record one request
//! left behind.
//!
//! Shared by the component and daemon-live roots, because both read the same
//! three facts about one response: the status the peer was given, the framing
//! the wire carried, and what the completion account recorded once its
//! finalizer settled.

use camber::http::{Request, Router, StreamResponse};
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use super::trace_capture::{TraceCapture, field_value};

/// The sentence one completed operation is recorded under.
pub const COMPLETION_EVENT: &str = "message=request completed";

/// The name the completion account publishes a download source failure under.
///
/// The boundary dimension is the account's download boundary. A body that
/// declared a length and whose source ended with bytes still owed is recorded
/// under this name, and no configured bound is.
pub const SOURCE_FAILURE_BOUNDARY: &str = "source_failure";

/// The declared red diagnostic for a clean early end with bytes still owed.
pub const TRUNCATION_DIAGNOSTIC: &str = "declared truncation did not record source failure";

/// The name every absent completion dimension is published under.
const ABSENT: &str = "none";

/// How long a peer read, a release, or a settled record may take.
pub const SOURCE_FAILURE_BOUND: Duration = Duration::from_secs(10);

/// How many body bytes a peer read retains.
const PEER_READ_LIMIT: usize = 64 * 1024;

/// The terminal chunk a chunked body ends cleanly with.
const TERMINAL_CHUNK: &[u8] = b"0\r\n\r\n";

/// What one held source produces, and when.
#[derive(Clone, Copy)]
pub struct HeldBody {
    /// The content length the head declares, or `None` for an unframed body.
    pub declared: Option<usize>,
    /// Bytes sent before the release.
    pub before: &'static [u8],
    /// Bytes sent after the release, before the source closes cleanly.
    pub after: &'static [u8],
}

/// The release a held source waits on before it finishes.
///
/// Dropping it closes the channel, so a failed row still lets the spawned
/// producer end inside its runtime.
pub struct Release(tokio::sync::mpsc::UnboundedSender<()>);

impl Release {
    /// Let the held source send what remains and close.
    pub fn release(&self, row: &str) {
        self.0
            .send(())
            .unwrap_or_else(|_| panic!("{row}: the held source was gone before its release"));
    }
}

/// Register a streaming route whose source waits for its release.
///
/// The source closes its sender cleanly after the release. It never reports an
/// error, so any failure the account records is one production inferred from
/// the declared length.
pub fn held_route(router: &mut Router, path: &str, body: HeldBody) -> Release {
    let (release, releases) = tokio::sync::mpsc::unbounded_channel::<()>();
    let releases = Arc::new(tokio::sync::Mutex::new(releases));
    router.get_stream(path, move |_req: &Request| {
        let releases = Arc::clone(&releases);
        Box::pin(async move {
            let (response, sender) = StreamResponse::new(200);
            tokio::spawn(async move {
                if !body.before.is_empty() {
                    drop(sender.send(body.before).await);
                }
                if releases.lock().await.recv().await.is_some() && !body.after.is_empty() {
                    drop(sender.send(body.after).await);
                }
                drop(sender);
            });
            match body.declared {
                Some(length) => response.with_header("content-length", &length.to_string()),
                None => response,
            }
        })
    });
    Release(release)
}

/// What one HTTP/1 peer read from a held response.
#[derive(Debug)]
pub struct PeerExchange {
    pub status: u16,
    pub declared: Option<usize>,
    pub chunked: bool,
    /// Every byte after the head, up to the transport's end.
    pub body: Box<[u8]>,
    /// Whether the transport ended in an error rather than a clean EOF.
    pub reset: bool,
}

/// Send one `GET`, read the head, call `release`, then read to the end.
///
/// The release happens only after the head has reached the peer. The status
/// the peer saw is therefore committed before the source ends.
pub fn exchange_after_head(
    addr: SocketAddr,
    path: &str,
    row: &str,
    release: impl FnOnce(),
) -> PeerExchange {
    exchange_after_prefix(addr, path, row, 0, release)
}

/// Read the head and `prefix_len` body bytes before releasing the source.
///
/// A source error can discard buffered transport bytes. Observing the prefix
/// at the peer orders its delivery before the source can fail.
pub fn exchange_after_prefix(
    addr: SocketAddr,
    path: &str,
    row: &str,
    prefix_len: usize,
    release: impl FnOnce(),
) -> PeerExchange {
    let mut stream = super::http::connect(addr)
        .unwrap_or_else(|error| panic!("{row}: the peer could not connect: {error}"));
    super::http::write_request(&mut stream, "GET", path, &[], &[])
        .unwrap_or_else(|error| panic!("{row}: the peer could not send: {error}"));
    let head = super::http::read_head(&mut stream, SOURCE_FAILURE_BOUND)
        .unwrap_or_else(|error| panic!("{row}: no whole head arrived: {error}"));
    let (status, declared, chunked) = parse_head(&head, row);
    let mut body = Vec::new();
    super::http::with_read_deadline(&mut stream, SOURCE_FAILURE_BOUND, |stream, deadline| {
        super::http::read_to_length(
            stream,
            &mut body,
            prefix_len,
            PEER_READ_LIMIT,
            "held source prefix",
            Some(deadline),
        )
    })
    .unwrap_or_else(|error| panic!("{row}: the prefix did not arrive before release: {error}"));
    release();
    let (body, reset) = read_to_end(&mut stream, body, row);
    PeerExchange {
        status,
        declared,
        chunked,
        body,
        reset,
    }
}

/// The status, the declared length, and the chunked coding one head names.
fn parse_head(head: &[u8], row: &str) -> (u16, Option<usize>, bool) {
    let text = String::from_utf8_lossy(head);
    let status = super::http::status_from_raw(&text);
    // Every declared length is parsed, so a malformed one fails the row even
    // when a later one would stand.
    let declared = super::http::header_values(&text, "content-length")
        .map(|value| declared_length(value, row))
        .last();
    let chunked = super::http::header_values(&text, "transfer-encoding")
        .last()
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    (status, declared, chunked)
}

/// One declared content-length value as a number.
///
/// A malformed length fails the row with its own text rather than reading as
/// an absent one.
pub fn declared_length(value: &str, row: &str) -> usize {
    value
        .parse()
        .unwrap_or_else(|_| panic!("{row}: the declared length was not a number: {value:?}"))
}

/// Read to the transport's end, and report whether that end was a reset.
///
/// A read deadline is not an end. It means the server never ended the body,
/// which no row here accepts. Only a gone peer counts as a reset; any other
/// transport fault fails the row rather than reading as one.
fn read_to_end(stream: &mut TcpStream, mut body: Vec<u8>, row: &str) -> (Box<[u8]>, bool) {
    let ended =
        super::http::with_read_deadline(stream, SOURCE_FAILURE_BOUND, |stream, deadline| {
            super::http::read_to_eof(stream, &mut body, PEER_READ_LIMIT, Some(deadline))
        });
    let reset = match ended {
        Ok(()) => false,
        Err(error) if super::http::is_closed_connection_error(&error) => true,
        Err(error) if super::http::is_deadline_expiry(&error) => {
            panic!("{row}: the server never ended the body: {error}")
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            panic!("{row}: the body exceeded the {PEER_READ_LIMIT}-byte fixture limit")
        }
        Err(error) => panic!("{row}: the body read failed: {error}"),
    };
    (body.into_boxed_slice(), reset)
}

/// The wire carried less than the head promised.
pub fn assert_incomplete_framing(exchange: &PeerExchange, row: &str) {
    match exchange.declared {
        Some(declared) => assert!(
            exchange.body.len() < declared,
            "{row}: the peer received all {declared} declared bytes: {exchange:?}"
        ),
        None => {
            assert!(
                exchange.chunked,
                "{row}: the head named neither a length nor chunked coding"
            );
            assert!(
                !exchange.body.ends_with(TERMINAL_CHUNK),
                "{row}: a truncated body was framed as a clean chunked end: {exchange:?}"
            );
        }
    }
}

/// The wire carried exactly what the head promised, and ended cleanly.
pub fn assert_complete_framing(exchange: &PeerExchange, expected: &[u8], row: &str) {
    assert!(
        !exchange.reset,
        "{row}: the transport failed under a whole body"
    );
    match exchange.declared {
        Some(declared) => {
            assert_eq!(declared, expected.len(), "{row}: the declared length");
            assert_eq!(
                exchange.body.as_ref(),
                expected,
                "{row}: the declared body arrived whole"
            );
        }
        None => {
            assert!(exchange.chunked, "{row}: an unframed body is chunked");
            assert!(
                exchange.body.ends_with(TERMINAL_CHUNK),
                "{row}: the unframed body ended with its terminal chunk: {exchange:?}"
            );
        }
    }
}

/// One local row whose source declares `declared` bytes, produces `produced`,
/// and closes cleanly after the peer has read the head and produced bytes.
///
/// The peer keeps the committed status, the wire stays incomplete, and the
/// completion account records the source failure.
pub fn declared_short_row(
    capture: &TraceCapture,
    addr: SocketAddr,
    path: &str,
    declared: usize,
    produced: &[u8],
    release: &Release,
    row: &str,
) {
    let exchange = exchange_after_prefix(addr, path, row, produced.len(), || release.release(row));
    assert_eq!(exchange.status, 200, "{row}: the committed status");
    assert_eq!(
        exchange.declared,
        Some(declared),
        "{row}: the head declared the source's length"
    );
    assert_eq!(
        exchange.body.as_ref(),
        produced,
        "{row}: the peer received exactly what the source produced"
    );
    assert_incomplete_framing(&exchange, row);
    assert_source_failure_completion(capture, path, row);
}

/// Require that `path`'s one completion kept its status and recorded a
/// non-normal download source failure.
pub fn assert_source_failure_completion(capture: &TraceCapture, path: &str, row: &str) {
    let record = one_completion_record(capture, path, row);
    assert_committed_status(&record, row);
    assert_not_normal(&record, row);
    assert_source_failure_recorded(&record, row);
}

/// Every completion record captured for `path`.
pub fn completion_records(capture: &TraceCapture, path: &str) -> Box<[Box<str>]> {
    capture
        .events()
        .into_iter()
        .filter(|event| records_path(event, path))
        .collect()
}

/// Whether one captured event is the completion record for `path`.
fn records_path(event: &str, path: &str) -> bool {
    field_value(event, "path") == Some(path)
}

/// Wait for the finalizer to settle `path`, then require exactly one record.
pub fn one_completion_record(capture: &TraceCapture, path: &str, row: &str) -> Box<str> {
    let settled = super::http::poll_until(SOURCE_FAILURE_BOUND, || {
        capture
            .events()
            .iter()
            .any(|event| records_path(event, path))
    });
    assert!(settled, "{row}: no completion record appeared for {path}");
    assert_one_record(capture, path, row)
}

/// Require exactly one record for `path`, and hand it back.
pub fn assert_one_record(capture: &TraceCapture, path: &str, row: &str) -> Box<str> {
    match Box::<[Box<str>; 1]>::try_from(completion_records(capture, path)) {
        Ok(one) => {
            let [record] = *one;
            record
        }
        Err(records) => panic!(
            "{row}: one request left {} completion records: {records:?}",
            records.len()
        ),
    }
}

/// Run every named row, even after one fails, then fail with every row's
/// failure.
///
/// A failed row cannot hide a later one: each row's own diagnostic reaches the
/// log, and the one failure this raises names them all.
pub fn run_every_row<'a, R: FnOnce()>(rows: impl IntoIterator<Item = (&'a str, R)>) {
    let mut ran = 0_usize;
    let failed: Box<[String]> = rows
        .into_iter()
        .filter_map(|(row, run)| {
            ran = ran.saturating_add(1);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
                .err()
                .map(|payload| format!("{row}: {}", super::http::panic_text(payload.as_ref())))
        })
        .collect();
    assert!(
        failed.is_empty(),
        "{} of {ran} rows failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}

/// The record keeps the status the peer was given.
pub fn assert_committed_status(record: &str, row: &str) {
    assert_eq!(
        field_value(record, "status"),
        Some("200"),
        "{row}: the record replaced the committed status: {record}"
    );
    assert_eq!(
        field_value(record, "rejection"),
        Some(ABSENT),
        "{row}: a post-commit end reached a rejection mapper: {record}"
    );
}

/// The record does not report normal completion.
pub fn assert_not_normal(record: &str, row: &str) {
    assert_eq!(
        field_value(record, "delivery"),
        Some("interrupted"),
        "{row}: a body that ended short was recorded as delivered: {record}"
    );
}

/// The record reports one normal completion with no crossed bound.
pub fn assert_normal(record: &str, row: &str) {
    assert_committed_status(record, row);
    assert_eq!(
        field_value(record, "delivery"),
        Some("produced"),
        "{row}: a whole body was not recorded as delivered: {record}"
    );
    assert_eq!(
        field_value(record, "boundary"),
        Some(ABSENT),
        "{row}: a whole body was recorded under a boundary: {record}"
    );
}

/// The completion account recorded the source failure as its download
/// boundary.
pub fn assert_source_failure_recorded(record: &str, row: &str) {
    assert_eq!(
        field_value(record, "boundary"),
        Some(SOURCE_FAILURE_BOUNDARY),
        "{TRUNCATION_DIAGNOSTIC}: {row}: {record}"
    );
}
