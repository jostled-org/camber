//! What a real peer sees when a streaming multipart handler stops reading.
//!
//! Every case here binds a listener, speaks HTTP/1.1 on a raw socket or HTTP/2
//! on one persistent connection, and reads its claims off the production
//! session's own counters. The command boundary is the subject. A handler that
//! is not asking for bytes has to stop the peer's upload where the transport
//! stops it — a stalled socket write on HTTP/1, withheld flow-control credit on
//! HTTP/2 — and every way a request can end has to release the incoming body,
//! the one source frame, the parser buffers, and the admitted permit before
//! anything answers.
//!
//! Nothing here waits on a clock for a rendezvous. Arrivals are polled until
//! they happen, and the three claims that are about something *not* happening —
//! a socket that stops taking bytes, a stream that is granted no credit, and a
//! graceful stop that has not returned — are read off a bounded quiet window and
//! paired with the positive claim that follows: the upload resumes the moment
//! the hold is released, and the stop returns once its session has answered.

use crate::h2_client::{H2Offer, H2RequestStream, PersistentH2Client};
use crate::http as wire;
use crate::multipart_support::{
    BOUNDARY, Escape, Escapes, Field, HandlerFuture, assert_escaped_inert, content_type,
    drain_field, drain_fields, escaping_handler, failing_handler, multipart_body, reading_handler,
    silent_handler, upload,
};
use crate::rejection_support::{Journal, Observed, drain, journal, only, recording_mapper};

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, MultipartObservation};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, Method, MultipartField, MultipartLimits, MultipartStream,
    RejectionKind, Request, Response, Router,
};
use std::io::{self, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;

/// How long any connect, answer, arrival, or shutdown here has to settle.
const BOUND: Duration = Duration::from_secs(10);

/// How long a claim that nothing happens is given to be wrong.
///
/// A negative claim has no arrival to poll for, so it is read off a settle
/// window. Every case that uses one pairs it with the positive claim that
/// follows — the peer makes progress the moment the hold is released — so the
/// window bounds the observation rather than deciding it.
const QUIET: Duration = Duration::from_millis(500);

/// The most one delivered chunk may carry.
const CHUNK_BYTES: usize = 64 * 1024;

/// The most one request may carry, and the most one field of it may.
const ADMITTED: usize = 8 * 1024 * 1024;

/// The one route whose admitted total a peer can reach.
///
/// Every other route here admits [`ADMITTED`], which is past every body these
/// cases frame — so a refusal on this one is the admitted total and can be
/// nothing else.
const TIGHT_ROUTE: &str = "/tight";

/// The most the tight route's peer may send in total.
const TIGHT_ADMITTED: usize = 1024;

/// One chunked data frame of a body that runs past the admitted total.
const TIGHT_FRAME: usize = 256;

/// The payload a body sends to cross [`TIGHT_ADMITTED`].
///
/// Twice the ceiling, so the crossing happens while the session is reading
/// rather than at the last byte of the body.
const OVER_TOTAL_BYTES: usize = 2 * TIGHT_ADMITTED;

/// The payload one generated HTTP/1 transfer chunk carries.
///
/// The HTTP/1 backpressure case generates these chunks until the real socket
/// refuses another write, so it does not assume a platform-specific buffer
/// ceiling or allocate a request sized to one.
const HTTP1_STREAM_CHUNK_BYTES: usize = 64 * 1024;

/// The most one non-blocking producer offer generates before yielding.
const HTTP1_OFFER_BYTES: usize = 1024 * 1024;

/// The test route's explicit field and request ceiling.
const HTTP1_STREAM_MAX_BYTES: usize = 256 * 1024 * 1024;

/// The fixed payload used by held rows that do not discover a socket plateau.
const HELD_BYTES: usize = 4 * 1024 * 1024;

/// One HTTP/2 request-body frame.
const H2_FRAME: usize = 16 * 1024;

/// How much of a held body the HTTP/1 cases send before they stop.
///
/// Enough for the first field's headers and one chunk, and small enough that a
/// blocking write of it cannot stall against any transport buffer.
const PREFIX: usize = 8 * 1024;

/// The most one small-limit field may carry.
const SMALL_FIELD_BYTES: usize = 512;

/// The authority every request here is addressed to.
const HOST: &str = "localhost";

/// The origin the fixture's rejection mapper records itself under.
const LIVE: &str = "live";

/// The limits every full-size route here registers.
fn held_limits() -> MultipartLimits {
    MultipartLimits::builder()
        .max_fields(4)
        .max_field_bytes(HTTP1_STREAM_MAX_BYTES)
        .max_headers_per_field(8)
        .max_header_bytes_per_field(1024)
        .max_chunk_bytes(CHUNK_BYTES)
        .max_parser_buffer_bytes(256 * 1024)
        .build()
        .expect("the held fixture limits are a valid combination")
}

/// The limits the one byte-limit route registers.
fn small_limits() -> MultipartLimits {
    MultipartLimits::builder()
        .max_fields(4)
        .max_field_bytes(SMALL_FIELD_BYTES)
        .max_headers_per_field(8)
        .max_header_bytes_per_field(1024)
        .max_chunk_bytes(64)
        .max_parser_buffer_bytes(4096)
        .build()
        .expect("the small fixture limits are a valid combination")
}

/// The hold one paused handler waits at, and the arrival a case reads off it.
///
/// The wake is registered before the arrival is published, so a case that
/// releases this gate the instant it sees the count cannot lose the wake it
/// sent.
#[derive(Default)]
struct Gate {
    reached: AtomicUsize,
    released: tokio::sync::Notify,
}

impl Gate {
    /// Hold here until the case releases this gate.
    async fn hold(&self) {
        let mut waiting = std::pin::pin!(self.released.notified());
        waiting.as_mut().enable();
        self.reached.fetch_add(1, Ordering::SeqCst);
        waiting.await;
    }

    /// How many handlers have reached this hold.
    fn reached(&self) -> usize {
        self.reached.load(Ordering::SeqCst)
    }

    /// Release every handler holding here.
    fn release(&self) {
        self.released.notify_waiters();
    }
}

/// Wait for `ready` off this runtime, and report whether it arrived in `bound`.
///
/// [`wire::poll_until`] sleeps the thread it polls on, so an `async fn` calling
/// it directly holds a runtime worker for the whole bound — including a worker
/// the server, the connection driver, or the handler being waited on needs in
/// order to reach the arrival. On a machine with few workers that turns a
/// bounded wait into a bound-length stall and then a failure. Every arrival here
/// is therefore polled from the blocking pool instead.
async fn arrived(bound: Duration, ready: impl FnMut() -> bool + Send + 'static) -> bool {
    tokio::task::spawn_blocking(move || wire::poll_until(bound, ready))
        .await
        .expect("the bounded arrival was waited for")
}

/// What the held route's handlers leave behind for a case to read.
///
/// Two counts rather than one, because the whole difference between a handler
/// that was cancelled and every other terminal is which of them moved: a
/// handler dropped where it waits raises the first and never the second.
#[derive(Default)]
struct Held {
    dropped: AtomicUsize,
    resumed: AtomicUsize,
}

impl Held {
    /// How many held handler futures have been dropped, wherever they stood.
    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    /// How many held handlers passed their hold and went on reading.
    fn resumed(&self) -> usize {
        self.resumed.load(Ordering::SeqCst)
    }
}

/// The witness one held handler owns for exactly as long as its future exists.
struct HeldGuard(Arc<Held>);

impl Drop for HeldGuard {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// Everything one served fixture owns, and what its cases read their claims off.
struct Fixture {
    gate: Arc<Gate>,
    /// Counts every admitted permit's own release, one per request.
    released: Arc<AtomicUsize>,
    /// Holds the access handles the escaping routes hand out.
    escapes: Arc<Escapes>,
    /// Counts the reading route's invocations.
    handled: Arc<AtomicUsize>,
    /// What became of the handlers that reached the hold.
    held: Arc<Held>,
    mapped: Journal,
}

impl Fixture {
    /// A fixture with nothing recorded yet.
    fn new() -> Self {
        Self {
            gate: Arc::new(Gate::default()),
            released: Arc::new(AtomicUsize::new(0)),
            escapes: Escapes::new(),
            handled: Arc::new(AtomicUsize::new(0)),
            held: Arc::new(Held::default()),
            mapped: journal(),
        }
    }
}

/// The router every case here is served through.
///
/// One registration per terminal these cases reach, plus one ordinary route so
/// an HTTP/2 case can complete a second stream while the first has no credit.
fn live_router(fixture: &Fixture) -> Router {
    let permits = Arc::clone(&fixture.released);
    let mut router = Router::new().max_request_body(HTTP1_STREAM_MAX_BYTES);
    router.multipart(
        Method::Post,
        "/hold",
        held_limits(),
        holding_handler(&fixture.gate, &fixture.held),
    );
    router.multipart(
        Method::Post,
        "/read",
        held_limits(),
        reading_handler("read", "absent", &fixture.handled),
    );
    router.multipart(
        Method::Post,
        "/small",
        small_limits(),
        reading_handler("small", "absent", &fixture.handled),
    );
    router.multipart(
        Method::Post,
        TIGHT_ROUTE,
        held_limits(),
        reading_handler("tight", "absent", &fixture.handled),
    );
    router.multipart(Method::Post, "/decline", held_limits(), failing_handler);
    router.multipart(Method::Post, "/silent", held_limits(), silent_handler);
    router.multipart(
        Method::Post,
        "/cancel-error",
        held_limits(),
        cancelling_handler(&fixture.gate, Cancelled::Report),
    );
    router.multipart(
        Method::Post,
        "/cancel-retry",
        held_limits(),
        cancelling_handler(&fixture.gate, Cancelled::Retry),
    );
    router.multipart(
        Method::Post,
        "/channel",
        held_limits(),
        escaping_handler(Escape::Channel, &fixture.escapes),
    );
    router.multipart(
        Method::Post,
        "/task",
        held_limits(),
        escaping_handler(Escape::Task, &fixture.escapes),
    );
    router.get("/control", |_request: &Request| async move {
        Response::text(200, "control")
    });
    router
        .body_admission(move |context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                admitted_for(context.route()),
                wire::permit_probe(&permits),
            ))
        })
        .rejection_mapper(recording_mapper(&fixture.mapped, LIVE))
}

/// The total one route's peer may send.
///
/// One route admits a ceiling a body can reach and every other admits one no
/// body here comes near, so a refusal for the admitted total is that route's
/// and cannot be confused with a field, a delimiter, or a declaration.
fn admitted_for(route: &str) -> usize {
    match route {
        TIGHT_ROUTE => TIGHT_ADMITTED,
        "/hold" => HTTP1_STREAM_MAX_BYTES,
        _ => ADMITTED,
    }
}

/// A handler that reads one chunk, holds, and then drains everything left.
fn holding_handler(
    gate: &Arc<Gate>,
    held: &Arc<Held>,
) -> impl Fn(&Request, MultipartStream) -> HandlerFuture + Send + Sync + 'static {
    let gate = Arc::clone(gate);
    let held = Arc::clone(held);
    move |_request: &Request, stream: MultipartStream| {
        let gate = Arc::clone(&gate);
        let held = Arc::clone(&held);
        Box::pin(held_read(stream, gate, held))
    }
}

/// Read one chunk, hold, then read everything the peer still owes.
///
/// The witness lives as long as this future does, so a case that never releases
/// the hold learns from it that the future was dropped where it waited.
async fn held_read(
    mut stream: MultipartStream,
    gate: Arc<Gate>,
    held: Arc<Held>,
) -> Result<Response, RuntimeError> {
    let _witness = HeldGuard(Arc::clone(&held));
    let bytes = held_field(&mut stream, &gate, &held).await? + drain_fields(&mut stream).await?;
    Response::text(200, &format!("held {bytes}"))
}

/// Take one chunk of the first field, hold at `gate`, then finish that field.
async fn held_field(
    stream: &mut MultipartStream,
    gate: &Gate,
    held: &Held,
) -> Result<usize, RuntimeError> {
    let mut field = stream.next_field().await?.ok_or_else(no_field)?;
    let first = field.next_chunk().await?.map_or(0, |chunk| chunk.len());
    gate.hold().await;
    held.resumed.fetch_add(1, Ordering::SeqCst);
    Ok(first + drain_field(&mut field).await?)
}

/// The failure a held route reports when its peer framed no field at all.
fn no_field() -> RuntimeError {
    RuntimeError::BadRequest("the held upload declared no field".into())
}

/// What a handler does once its accepted read has been cancelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Cancelled {
    /// Report the cancellation as the handler's own failure.
    Report,
    /// Try one more operation, which must be refused, and answer anyway.
    Retry,
}

/// A handler whose accepted read is dropped where it waits.
fn cancelling_handler(
    gate: &Arc<Gate>,
    after: Cancelled,
) -> impl Fn(&Request, MultipartStream) -> HandlerFuture + Send + Sync + 'static {
    let gate = Arc::clone(gate);
    move |_request: &Request, stream: MultipartStream| {
        let gate = Arc::clone(&gate);
        Box::pin(cancelled_read(stream, gate, after))
    }
}

/// Read one chunk, lose an accepted read, and answer as `after` states.
async fn cancelled_read(
    mut stream: MultipartStream,
    gate: Arc<Gate>,
    after: Cancelled,
) -> Result<Response, RuntimeError> {
    let mut field = stream.next_field().await?.ok_or_else(no_field)?;
    field.next_chunk().await?;
    cancel_pending(&mut field, &gate).await;
    match after {
        Cancelled::Report => Err(RuntimeError::BadRequest(
            "the handler cancelled its own read".into(),
        )),
        Cancelled::Retry => {
            let refused = field.next_chunk().await.is_err();
            Response::text(200, &format!("refused={refused}"))
        }
    }
}

/// Issue one read, let the driver accept it, and drop it where it waits.
///
/// The first poll is what puts the command in flight; the peer is withholding
/// the rest of its payload, so nothing can answer it, and the hold is what ends
/// this function — taking the borrowed read with it.
async fn cancel_pending(field: &mut MultipartField<'_>, gate: &Gate) {
    let mut reading = std::pin::pin!(field.next_chunk());
    let issued = std::future::poll_fn(|cx| Poll::Ready(reading.as_mut().poll(cx))).await;
    assert!(
        issued.is_pending(),
        "a withheld payload cannot complete this read"
    );
    gate.hold().await;
}

/// The body every held case sends.
fn held_body() -> Box<[u8]> {
    multipart_body(BOUNDARY, &[Field::bytes("upload", &[7u8; HELD_BYTES])]).into_boxed_slice()
}

/// A valid, complete body every ordinary row sends.
fn valid_body() -> Box<[u8]> {
    multipart_body(BOUNDARY, &[Field::text("note", "hello")]).into_boxed_slice()
}

/// The multipart representation every request here declares.
fn declared() -> Box<str> {
    content_type(BOUNDARY)
}

/// One counted chunked request that grows only until its real socket stalls.
struct Producer {
    pending: Box<[u8]>,
    offset: usize,
    sent: usize,
    field_bytes: usize,
    phase: ProducerPhase,
}

impl Producer {
    /// Start one chunked multipart request with its first generated data frame.
    fn new() -> Self {
        let head = wire::framed_chunked_request_head(
            wire::CLOSE_AFTER_RESPONSE,
            "POST",
            "/hold",
            &[("Content-Type", declared().as_ref())],
        );
        Self {
            pending: first_stream_frame(&head),
            offset: 0,
            sent: 0,
            field_bytes: HTTP1_STREAM_CHUNK_BYTES,
            phase: ProducerPhase::Streaming,
        }
    }

    /// How many bytes the socket has taken.
    fn sent(&self) -> usize {
        self.sent
    }

    /// How many bytes remain in the frame the socket refused.
    fn pending(&self) -> usize {
        self.pending.len() - self.offset
    }

    /// How many field bytes this request will contain once sealed.
    fn field_bytes(&self) -> usize {
        self.field_bytes
    }

    /// Stop generating data after the currently pending frame.
    fn seal(&mut self) {
        self.phase = ProducerPhase::Closing;
    }

    /// Whether the closing delimiter and chunk terminator were sent.
    fn complete(&self) -> bool {
        matches!(self.phase, ProducerPhase::Finished) && self.pending() == 0
    }

    /// Offer a bounded generated batch, stopping at the first socket refusal.
    fn offer(&mut self, socket: &mut TcpStream) -> io::Result<()> {
        let started = self.sent;
        while self.sent - started < HTTP1_OFFER_BYTES {
            self.refill();
            if self.complete() {
                return Ok(());
            }
            match socket.write(&self.pending[self.offset..]) {
                Ok(0) => return Ok(()),
                Ok(taken) => {
                    self.offset += taken;
                    self.sent += taken;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Materialize only the next transfer frame the socket can be offered.
    fn refill(&mut self) {
        if self.pending() != 0 {
            return;
        }
        match self.phase {
            ProducerPhase::Streaming => {
                self.offset = 0;
                self.pending = stream_frame();
                self.field_bytes += HTTP1_STREAM_CHUNK_BYTES;
            }
            ProducerPhase::Closing => {
                self.offset = 0;
                self.pending = closing_stream_frame();
                self.phase = ProducerPhase::Finished;
            }
            ProducerPhase::Finished => {}
        }
    }
}

/// Which bytes the counted HTTP/1 producer may generate next.
#[derive(Debug)]
enum ProducerPhase {
    Streaming,
    Closing,
    Finished,
}

/// Frame the multipart prefix and first repeated payload as one transfer chunk.
fn first_stream_frame(head: &[u8]) -> Box<[u8]> {
    let mut payload =
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"upload\"\r\n\r\n")
            .into_bytes();
    payload.resize(payload.len() + HTTP1_STREAM_CHUNK_BYTES, 7);
    let mut framed = head.to_vec();
    framed.extend_from_slice(&transfer_chunk(&payload));
    framed.into_boxed_slice()
}

/// Frame one generated payload block as one HTTP transfer chunk.
fn stream_frame() -> Box<[u8]> {
    transfer_chunk(&[7; HTTP1_STREAM_CHUNK_BYTES])
}

/// Frame the multipart end and the HTTP chunked terminator.
fn closing_stream_frame() -> Box<[u8]> {
    let closing = format!("\r\n--{BOUNDARY}--\r\n");
    let mut framed = transfer_chunk(closing.as_bytes()).into_vec();
    framed.extend_from_slice(b"0\r\n\r\n");
    framed.into_boxed_slice()
}

/// Put one payload behind its hexadecimal transfer length.
fn transfer_chunk(payload: &[u8]) -> Box<[u8]> {
    let mut framed = format!("{:x}\r\n", payload.len()).into_bytes();
    framed.extend_from_slice(payload);
    framed.extend_from_slice(b"\r\n");
    framed.into_boxed_slice()
}

/// Keep offering pending bytes until `ready`, and report whether it arrived.
fn offer_until(
    producer: &mut Producer,
    socket: &mut TcpStream,
    bound: Duration,
    mut ready: impl FnMut(&Producer) -> bool,
) -> bool {
    wire::poll_until(bound, || {
        producer.offer(socket).unwrap_or_else(|error| {
            panic!(
                "the client could not offer its pending request bytes: {error}; sent={}, field={}, pending={}, phase={:?}",
                producer.sent,
                producer.field_bytes,
                producer.pending(),
                producer.phase
            )
        });
        ready(producer)
    })
}

/// Offer pending bytes until the transport stops taking them for a whole quiet
/// window.
///
/// The buffers between the two peers do not fill the instant a handler stops
/// reading — the kernel keeps taking bytes until every one of them is full, and
/// how many that is belongs to the operating system rather than to this case.
/// So the plateau is found rather than assumed, and the claim above it is that
/// the plateau holds while bytes are still owed.
fn stall(producer: &mut Producer, socket: &mut TcpStream) -> bool {
    wire::poll_until(BOUND, || {
        let before = producer.sent();
        !offer_until(producer, socket, QUIET, |producer| {
            producer.sent() != before
        })
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn http1_slow_consumer_stops_socket_progress_within_session_bounds() {
    let fixture = Fixture::new();
    let port = wire::reserve_observed();
    let controller = port.controller();
    let server = port.serve(live_router(&fixture));

    let mut socket = wire::connect(server.addr()).expect("the held fixture connected");
    socket
        .set_nonblocking(true)
        .expect("the client socket refuses no room instead of waiting for it");
    let mut producer = Producer::new();
    assert!(
        offer_until(&mut producer, &mut socket, BOUND, |_| fixture
            .gate
            .reached()
            == 1),
        "the handler took its first chunk and holds: {} bytes accepted",
        producer.sent()
    );

    assert!(
        stall(&mut producer, &mut socket),
        "the transport stops taking bytes the handler is not asking for: {} bytes accepted",
        producer.sent()
    );

    let stalled = producer.sent();
    let held = controller.multipart_observed();
    let progressed = offer_until(&mut producer, &mut socket, QUIET, |producer| {
        producer.sent() != stalled
    });
    assert!(
        !progressed,
        "a handler that is not reading stops the peer's upload: {} bytes accepted, was {stalled}",
        producer.sent()
    );
    assert!(
        producer.pending() > 0,
        "the socket refused no pending payload while the handler held"
    );
    assert_held_within_session_bounds(&controller, held);

    producer.seal();
    let field_bytes = producer.field_bytes();
    fixture.gate.release();
    assert!(
        offer_until(&mut producer, &mut socket, BOUND, Producer::complete),
        "the peer's upload resumes once the hold is released: {} bytes still owed",
        producer.pending()
    );
    socket
        .set_nonblocking(false)
        .expect("the client socket waits for its answer");
    let answered =
        wire::read_http_response_bounded(&mut socket).expect("the held upload was answered");
    assert_eq!(
        (answered.status, String::from_utf8_lossy(&answered.body)),
        (200, format!("held {field_bytes}").into()),
        "the handler read every byte the peer owed"
    );
    wire::assert_released(&fixture.released, 1, "the held session released its permit");
    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}

/// Assert a held session polled nothing further and retained nothing past its
/// own formula.
fn assert_held_within_session_bounds(controller: &LifecycleController, held: MultipartObservation) {
    let limits = held_limits();
    let during = controller.multipart_observed();
    assert_eq!(
        during.body_frames_polled(),
        held.body_frames_polled(),
        "a held session polls no further payload frame: {during:?}"
    );
    assert!(
        during.parser_retained_bytes() <= limits.max_parser_buffer_bytes(),
        "the parser holds no more than its configured buffer: {during:?}"
    );
    assert!(
        during.parser_peak_bytes() <= limits.max_parser_buffer_bytes(),
        "the parser never held more than its configured buffer: {during:?}"
    );
    assert!(
        during.reply_peak_bytes() <= limits.max_reply_bytes(),
        "no reply payload exceeded the session's reply bound: {during:?}"
    );
    assert!(
        during.active_metadata_peak_bytes() <= limits.max_header_bytes_per_field(),
        "active field metadata stays inside one field's header bound: {during:?}"
    );
}

/// Offer the next frame of `body`, and report what the peer did with it.
async fn offer_frame(
    upload: &mut H2RequestStream,
    body: &[u8],
    offered: &mut usize,
    bound: Duration,
) -> H2Offer {
    let end = (*offered + H2_FRAME).min(body.len());
    let offer = upload.offer(&body[*offered..end], bound).await;
    match offer {
        H2Offer::Sent => *offered = end,
        H2Offer::Withheld | H2Offer::PeerStopped => {}
    }
    offer
}

/// Offer frames until `ready`, and report whether it arrived while the peer was
/// still taking payload.
async fn offer_frames_until(
    upload: &mut H2RequestStream,
    body: &[u8],
    offered: &mut usize,
    mut ready: impl FnMut() -> bool,
) -> bool {
    while !ready() {
        if *offered == body.len() {
            return false;
        }
        match offer_frame(upload, body, offered, BOUND).await {
            H2Offer::Sent => {}
            H2Offer::Withheld | H2Offer::PeerStopped => return ready(),
        }
    }
    true
}

/// Offer every frame of `body` the peer has not taken yet, requiring each.
///
/// The negative claim is behind this call, not in it: a case reaches here after
/// releasing its hold, and every frame the peer now refuses is credit that never
/// resumed.
async fn offer_rest(upload: &mut H2RequestStream, body: &[u8], offered: &mut usize) {
    while *offered < body.len() {
        let granted = *offered;
        match offer_frame(upload, body, offered, BOUND).await {
            H2Offer::Sent => {}
            refused => panic!(
                "credit did not resume after the hold was released: {refused:?} at {granted} of {} bytes",
                body.len()
            ),
        }
    }
}

/// Open one paced HTTP/2 upload to `path`.
async fn open_upload(client: &mut PersistentH2Client, path: &str) -> H2RequestStream {
    client
        .open_paced("POST", path, HOST, &[("content-type", declared().as_ref())])
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn http2_slow_consumer_withholds_flow_control_credit() {
    let fixture = Fixture::new();
    let port = wire::reserve_observed();
    let controller = port.controller();
    let server = port.serve(live_router(&fixture));

    let body = held_body();
    let mut client = PersistentH2Client::connect(server.addr(), BOUND).await;
    let mut upload = open_upload(&mut client, "/hold").await;
    let mut offered = 0;
    assert!(
        offer_frames_until(&mut upload, &body, &mut offered, || fixture.gate.reached()
            == 1)
        .await,
        "the handler took its first chunk and holds: {offered} bytes granted"
    );

    // Keep offering until the window the peer opened for this stream is spent.
    // Hyper grants a megabyte per request stream, and a handler that is not
    // reading releases none of it back.
    let withheld = loop {
        match offer_frame(&mut upload, &body, &mut offered, QUIET).await {
            H2Offer::Withheld => break true,
            H2Offer::PeerStopped => break false,
            H2Offer::Sent if offered == body.len() => break false,
            H2Offer::Sent => {}
        }
    };
    assert!(
        withheld,
        "a held handler leaves this stream without credit: {offered} bytes granted"
    );
    let held = controller.multipart_observed();

    // Same connection, second stream: the backpressure belongs to the held
    // stream, not to the connection carrying it.
    let control = client
        .send_complete("GET", "/control", HOST, &[], b"")
        .await;
    assert_eq!(
        (control.status, String::from_utf8_lossy(&control.body)),
        (200, "control".into()),
        "an ordinary stream completes while the held one has no credit"
    );
    assert_held_within_session_bounds(&controller, held);

    fixture.gate.release();
    offer_rest(&mut upload, &body, &mut offered).await;
    upload.finish();
    let answered = upload.answer().await;
    assert_eq!(
        (answered.status, String::from_utf8_lossy(&answered.body)),
        (200, format!("held {HELD_BYTES}").into()),
        "the handler read every byte the peer owed"
    );
    client.close().await;
    wire::assert_released(
        &fixture.released,
        2,
        "the held session and the control stream each released one permit",
    );
    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}

/// One refusal row: what the peer sends, and how the answer must be classified.
struct Refusal {
    label: &'static str,
    path: &'static str,
    body: RefusalBody,
    status: u16,
    kind: RejectionKind,
}

/// What one refusal row puts on the wire.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefusalBody {
    /// A declaration that states no boundary at all.
    NoBoundary,
    /// A valid body whose opening delimiter is not this request's boundary.
    WrongDelimiter,
    /// A field larger than the route's per-field maximum.
    OversizeField,
    /// A payload larger than the route's admitted total.
    OverTotal,
    /// A valid body a handler answers without reading.
    Unread,
    /// A valid body a handler declines without reading.
    Declined,
    /// A chunked body whose framing stops being readable.
    Unreadable,
}

impl RefusalBody {
    /// The representation this row declares.
    fn declared(self) -> Box<str> {
        match self {
            Self::NoBoundary => "multipart/form-data".into(),
            _ => declared(),
        }
    }

    /// The payload this row frames behind that declaration.
    fn wire(self) -> Box<[u8]> {
        match self {
            Self::WrongDelimiter => {
                multipart_body("NotTheBoundary", &[Field::text("a", "b")]).into_boxed_slice()
            }
            Self::OversizeField => multipart_body(
                BOUNDARY,
                &[Field::bytes("big", &[3u8; SMALL_FIELD_BYTES + 64])],
            )
            .into_boxed_slice(),
            Self::OverTotal => {
                multipart_body(BOUNDARY, &[Field::bytes("big", &[4u8; OVER_TOTAL_BYTES])])
                    .into_boxed_slice()
            }
            _ => valid_body(),
        }
    }
}

/// Every refusal an HTTP/1 peer can provoke on this listener.
const REFUSALS: &[Refusal] = &[
    Refusal {
        label: "a declaration without a boundary is a multipart failure",
        path: "/read",
        body: RefusalBody::NoBoundary,
        status: 400,
        kind: RejectionKind::Multipart,
    },
    Refusal {
        label: "a delimiter that frames nothing is a multipart failure",
        path: "/read",
        body: RefusalBody::WrongDelimiter,
        status: 400,
        kind: RejectionKind::Multipart,
    },
    Refusal {
        label: "a field past its configured maximum is a byte limit",
        path: "/small",
        body: RefusalBody::OversizeField,
        status: 413,
        kind: RejectionKind::BodyLimit,
    },
    Refusal {
        label: "a payload past the admitted total is a byte limit",
        path: TIGHT_ROUTE,
        body: RefusalBody::OverTotal,
        status: 413,
        kind: RejectionKind::BodyLimit,
    },
    Refusal {
        label: "a handler that read nothing leaves an incomplete body",
        path: "/silent",
        body: RefusalBody::Unread,
        status: 400,
        kind: RejectionKind::Multipart,
    },
    Refusal {
        label: "a handler that declined without reading keeps its own category",
        path: "/decline",
        body: RefusalBody::Declined,
        status: 400,
        kind: RejectionKind::Application,
    },
    Refusal {
        label: "a transport that stops delivering is unreadable",
        path: "/read",
        body: RefusalBody::Unreadable,
        status: 400,
        kind: RejectionKind::BodyUnreadable,
    },
];

/// Send one refusal row on a connection the peer offered to keep, and hand back
/// both the answer and the connection it was framed on.
fn send_refusal(addr: SocketAddr, row: &Refusal) -> (wire::HttpResponse, TcpStream) {
    let label = row.label;
    let declared = row.body.declared();
    match row.body {
        RefusalBody::Unreadable => wire::send_unreadable_body(
            addr,
            wire::KEEP_CONNECTION,
            "POST",
            row.path,
            declared.as_ref(),
        )
        .unwrap_or_else(|error| panic!("{label}: the unreadable body was not answered: {error}")),
        RefusalBody::OverTotal => send_over_total(addr, row.path, &row.body.wire()),
        _ => {
            let mut socket = wire::connect(addr)
                .unwrap_or_else(|error| panic!("{label}: could not connect: {error}"));
            wire::write_request_with_connection(
                &mut socket,
                wire::KEEP_CONNECTION,
                "POST",
                row.path,
                &[("Content-Type", declared.as_ref())],
                &row.body.wire(),
            )
            .unwrap_or_else(|error| panic!("{label}: could not send: {error}"));
            let answered = wire::read_http_response_bounded(&mut socket)
                .unwrap_or_else(|error| panic!("{label}: no response: {error}"));
            (answered, socket)
        }
    }
}

/// Send one body past the admitted total without declaring its length.
///
/// Chunked, because a declaration above the admitted maximum is refused from
/// the declaration alone and never reaches a session at all. What this row is
/// about is the total a running session measures as it reads, so the peer
/// states no length and the crossing happens mid-body.
fn send_over_total(addr: SocketAddr, path: &str, body: &[u8]) -> (wire::HttpResponse, TcpStream) {
    let declared = declared();
    let mut socket = wire::connect(addr).expect("the chunked client connected");
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {HOST}\r\nConnection: {}\r\n\
         Content-Type: {declared}\r\nTransfer-Encoding: chunked\r\n\r\n",
        wire::KEEP_CONNECTION
    );
    socket
        .write_all(head.as_bytes())
        .and_then(|()| socket.flush())
        .expect("the chunked head was sent");
    wire::tolerate_answered_peer(write_chunked(&mut socket, body))
        .expect("the chunked frames were sent, or the peer had already answered");
    let answered =
        wire::read_http_response_bounded(&mut socket).expect("the chunked upload was answered");
    (answered, socket)
}

/// Write one body as [`TIGHT_FRAME`]-sized data frames, then end it.
fn write_chunked(socket: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    for frame in body.chunks(TIGHT_FRAME) {
        wire::write_chunk(socket, frame)?;
    }
    wire::write_chunked_end(socket)
}

/// Assert one refusal row was classified once and states the category it must.
fn assert_refused_once(row: &Refusal, answered: &wire::HttpResponse, mapped: &Journal) {
    let label = row.label;
    assert_eq!(answered.status, row.status, "{label}: wire status");
    assert_classified_once(mapped, label, row.kind);
}

/// Assert one request was classified exactly once, and by whom.
fn assert_classified_once(mapped: &Journal, label: &str, kind: RejectionKind) {
    let observed = only(mapped, label);
    assert_eq!(
        (observed.origin, observed.kind),
        (LIVE, kind),
        "{label}: classified once by the listener's own mapper: {observed:?}"
    );
}

/// The route whose handler loses a read the driver had already accepted.
///
/// Abandonment is the one refusal no table row can carry: it is decided by what
/// the handler did with a read the peer had not answered yet, and a body that
/// arrived whole answers that read instead of leaving it to be lost. So both
/// halves below pace their own upload — enough payload for the first chunk, the
/// handler parked with its next read in flight, and the rest never sent.
const ABANDONED: &str = "/cancel-error";

/// Assert one abandoned session was answered the way abandonment is answered.
///
/// The handler reported the read it lost as its own failure, so the category
/// stays the handler's and only the disposition is the transport's — which is
/// what each half below asserts for itself.
fn assert_abandoned_answer(label: &str, answered: &wire::HttpResponse, fixture: &Fixture) {
    assert_eq!(answered.status, 400, "{label}: wire status");
    assert_classified_once(&fixture.mapped, label, RejectionKind::Application);
}

/// Assert an abandoned session ends the HTTP/1 connection it left payload on.
///
/// The peer offers to keep that connection, so the `close` in the answer is the
/// refusal's own disposition rather than the request's.
async fn assert_abandoned_http1_closes(addr: SocketAddr, fixture: &Fixture) {
    let label = "an abandoned session leaves an HTTP/1 payload unread";
    let held = fixture.gate.reached();
    let mut socket = send_prefix(addr, wire::KEEP_CONNECTION, ABANDONED, &held_body());
    let gate = Arc::clone(&fixture.gate);
    assert!(
        arrived(BOUND, move || gate.reached() == held + 1).await,
        "{label}: the handler holds with the read it loses in flight"
    );
    fixture.gate.release();

    let answered = wire::read_http_response_bounded(&mut socket)
        .unwrap_or_else(|error| panic!("{label}: no response: {error}"));
    assert_abandoned_answer(label, &answered, fixture);
    assert_eq!(
        answered.header("connection").map(str::to_ascii_lowercase),
        Some("close".to_owned()),
        "{label}: an HTTP/1 refusal over unread payload states close"
    );
    wire::assert_connection_closed(&mut socket, label);
}

/// Assert an abandoned session ends its own HTTP/2 stream and nothing else.
async fn assert_abandoned_h2_is_stream_local(client: &mut PersistentH2Client, fixture: &Fixture) {
    let label = "an abandoned session leaves an HTTP/2 payload unread";
    let held = fixture.gate.reached();
    let body = held_body();
    let mut upload = open_upload(client, ABANDONED).await;
    let mut offered = 0;
    assert_eq!(
        offer_frame(&mut upload, &body, &mut offered, BOUND).await,
        H2Offer::Sent,
        "{label}: the peer sent the one frame the handler's first read takes"
    );
    // The frames this row offered are flushed by a connection driver task on
    // this runtime, so the arrival is polled from a blocking thread rather than
    // from a worker that driver may need.
    let gate = Arc::clone(&fixture.gate);
    assert!(
        arrived(BOUND, move || gate.reached() == held + 1).await,
        "{label}: the handler holds with the read it loses in flight"
    );
    fixture.gate.release();

    let answered = upload.answer().await;
    upload.reset();
    assert_abandoned_answer(label, &answered, fixture);
    assert_eq!(
        answered.header("connection"),
        None,
        "{label}: HTTP/2 states no connection disposition"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_multipart_refusals_have_protocol_correct_disposition() {
    let fixture = Fixture::new();
    let port = wire::reserve_observed();
    let server = port.serve(live_router(&fixture));
    let addr = server.addr();

    for row in REFUSALS {
        let (answered, mut socket) = send_refusal(addr, row);
        assert_refused_once(row, &answered, &fixture.mapped);
        assert_eq!(
            answered.header("connection").map(str::to_ascii_lowercase),
            Some("close".to_owned()),
            "{}: an HTTP/1 refusal over unread payload states close",
            row.label
        );
        wire::assert_connection_closed(&mut socket, row.label);
    }
    assert_abandoned_http1_closes(addr, &fixture).await;

    assert_h2_refusals_are_stream_local(addr, &fixture).await;
    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}

/// Assert every refusal an HTTP/2 peer can provoke ends its own stream and no
/// more.
///
/// The unreadable row has no HTTP/2 spelling: request framing there is the
/// transport's own, and the only way to break it — resetting the stream —
/// destroys the answer this claim is read off. Every other row travels, and the
/// abandoned session that no row can carry travels behind them as its own paced
/// upload.
async fn assert_h2_refusals_are_stream_local(addr: SocketAddr, fixture: &Fixture) {
    let mut client = PersistentH2Client::connect(addr, BOUND).await;
    for row in REFUSALS {
        match row.body {
            RefusalBody::Unreadable => continue,
            _ => {}
        }
        let declared = row.body.declared();
        let answered = client
            .send_complete(
                "POST",
                row.path,
                HOST,
                &[("content-type", declared.as_ref())],
                &row.body.wire(),
            )
            .await;
        assert_refused_once(row, &answered, &fixture.mapped);
        assert_eq!(
            answered.header("connection"),
            None,
            "{}: HTTP/2 states no connection disposition",
            row.label
        );
    }
    assert_abandoned_h2_is_stream_local(&mut client, fixture).await;

    let answered = client
        .send_complete(
            "POST",
            "/read",
            HOST,
            &[("content-type", declared().as_ref())],
            &valid_body(),
        )
        .await;
    assert_eq!(
        answered.status, 200,
        "a later stream succeeds on the connection every refusal was framed on"
    );
    client.close().await;
}

/// One terminal a request reaches by sending a body and reading its answer.
struct Terminal {
    label: &'static str,
    path: &'static str,
    body: RefusalBody,
    status: u16,
    /// The category the mapper must record. `None` is the handler's own success.
    kind: Option<RejectionKind>,
    /// Whether this row's session ever asked the transport for payload.
    reads: bool,
}

/// How many drivers one terminal must have settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Settles {
    /// The session reached a terminal summary of its own.
    Once,
    /// The request future was dropped, so nothing settled this session.
    Never,
    /// The transport decides. A reset can fail the read in flight or tear the
    /// request down before that read is polled again; both release the same
    /// owners, and neither settles twice.
    Either,
}

impl Settles {
    /// Whether `settled` drivers is what this terminal permits.
    fn allows(self, settled: usize) -> bool {
        match self {
            Self::Once => settled == 1,
            Self::Never => settled == 0,
            Self::Either => settled <= 1,
        }
    }
}

/// What one terminal must have released by the time its request is over.
struct Ownership {
    /// Whether this row's session ever asked the transport for payload.
    reads: bool,
    /// What became of the driver that owned it.
    settles: Settles,
}

/// Assert one terminal released every owner its session held.
///
/// The permit is read through both of its counters — the application's own
/// probe and the production owner the listener counts — because a permit
/// released into one arithmetic and not the other is not released. The source
/// frames are the transport allocations the driver took: a session that polled
/// payload has to have let go of every one of them but the frame still in its
/// hand, and a session that polled none has nothing to release.
fn assert_ownership_released(
    label: &str,
    fixture: &Fixture,
    controller: &LifecycleController,
    before: MultipartObservation,
    released: usize,
    expected: &Ownership,
) {
    wire::assert_released(&fixture.released, released, label);
    wire::assert_owners_released(controller, released, label);
    let after = controller.multipart_observed();
    let settled = after.drivers_terminated() - before.drivers_terminated();
    assert!(
        expected.settles.allows(settled),
        "{label}: {settled} drivers returned a terminal summary, wanted {:?}: {after:?}",
        expected.settles
    );
    assert_sources_released(label, before, after, expected.reads);
}

/// Assert the source frames one session took are the ones it let go of.
///
/// The driver holds one source frame at a time and lets each go as the parser
/// spends it, so a session that has ended released every frame it polled except
/// at most the one it was still reading — the frame a structural or byte-limit
/// failure stops the parser inside of. Both ends are asserted: a released count
/// above what was polled is arithmetic nothing took, and one below that margin
/// is a transport allocation the session kept.
///
/// This is the served path, where the fixture owns no allocation witness and
/// [`MultipartObservation::source_frame_backings_freed`] answers `None`. The
/// claim here is the driver's own release of its handle and no more.
fn assert_sources_released(
    label: &str,
    before: MultipartObservation,
    after: MultipartObservation,
    reads: bool,
) {
    let polled = after.body_frames_polled() - before.body_frames_polled();
    let released = after.source_frames_released() - before.source_frames_released();
    match reads {
        true => assert!(
            polled > 0 && (polled.saturating_sub(1)..=polled).contains(&released),
            "{label}: released {released} of the {polled} source frames it polled"
        ),
        false => assert_eq!(
            (polled, released),
            (0, 0),
            "{label}: a session no handler asked anything of polls and releases nothing"
        ),
    }
}

/// Every terminal a complete request reaches without a transport fault.
const TERMINALS: &[Terminal] = &[
    Terminal {
        label: "a clean terminal commits the handler's own response",
        path: "/read",
        body: RefusalBody::Unread,
        status: 200,
        kind: None,
        reads: true,
    },
    Terminal {
        label: "a parser failure outranks the handler that carried it out",
        path: "/read",
        body: RefusalBody::WrongDelimiter,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        reads: true,
    },
    Terminal {
        label: "a byte-limit refusal outranks the handler that carried it out",
        path: "/small",
        body: RefusalBody::OversizeField,
        status: 413,
        kind: Some(RejectionKind::BodyLimit),
        reads: true,
    },
    Terminal {
        label: "a handler that answered without reading cannot commit it",
        path: "/silent",
        body: RefusalBody::Unread,
        status: 400,
        kind: Some(RejectionKind::Multipart),
        reads: false,
    },
    Terminal {
        label: "a handler that declined keeps its own category",
        path: "/decline",
        body: RefusalBody::Declined,
        status: 400,
        kind: Some(RejectionKind::Application),
        reads: false,
    },
];

/// Drive one terminal row and assert what it answered and what it released.
fn assert_terminal_row(
    row: &Terminal,
    addr: SocketAddr,
    fixture: &Fixture,
    controller: &LifecycleController,
    released: usize,
) {
    let label = row.label;
    let before = controller.multipart_observed();
    let answered = upload(addr, row.path, &row.body.wire());
    assert_eq!(answered.status, row.status, "{label}: wire status");
    // The answer is already off the wire, so whatever selected it has run: a
    // row that states no category has to leave the journal empty, or a handler's
    // own response was committed by a request that also ran the rejection
    // mapper.
    match row.kind {
        Some(kind) => assert_classified_once(&fixture.mapped, label, kind),
        None => {
            let observed = drain(&fixture.mapped);
            assert!(
                observed.is_empty(),
                "{label}: nothing was refused, so no mapper ran: {observed:?}"
            );
        }
    }
    assert_ownership_released(
        label,
        fixture,
        controller,
        before,
        released,
        &Ownership {
            reads: row.reads,
            settles: Settles::Once,
        },
    );
}

/// Send one request's head and only the first `PREFIX` bytes of its payload.
///
/// The declaration states the whole length, so the peer is owed the rest and the
/// session stays open on exactly the bytes a case chooses to withhold.
///
/// `connection` is the peer's own disposition, and a case whose claim is the
/// server's answer to it has to offer to keep the connection: a `close` the peer
/// asked for proves nothing about the one a refusal forces.
fn send_prefix(addr: SocketAddr, connection: &str, path: &str, body: &[u8]) -> TcpStream {
    let mut socket = wire::connect(addr).expect("the withholding client connected");
    let wire_bytes = wire::framed_request_bytes(
        connection,
        "POST",
        path,
        &[("Content-Type", declared().as_ref())],
        body,
    );
    let head = wire_bytes.len() - body.len();
    socket
        .write_all(&wire_bytes[..head + PREFIX])
        .and_then(|()| socket.flush())
        .expect("the withheld request's opening bytes were sent");
    socket
}

/// Assert a peer that goes away mid-upload releases the session it left behind.
///
/// The hold is released after the socket is gone, and that ordering is the
/// point. An HTTP/1 peer's departure is an end-of-stream on the request body,
/// which only whatever reads that body can observe — a handler parked on
/// something else learns nothing, because there is no frame to fail on until it
/// asks for one. So this row parks the handler to guarantee the session exists,
/// takes the peer away, and then lets the handler ask: the read fails on the
/// truncated payload, the request ends, and everything the session owned goes
/// with it even though no answer can ever be delivered.
async fn assert_disconnect_releases(
    addr: SocketAddr,
    fixture: &Fixture,
    controller: &LifecycleController,
    released: usize,
) {
    let before = controller.multipart_observed();
    let held = fixture.gate.reached();
    let socket = send_prefix(addr, wire::CLOSE_AFTER_RESPONSE, "/hold", &held_body());
    let gate = Arc::clone(&fixture.gate);
    assert!(
        arrived(BOUND, move || gate.reached() == held + 1).await,
        "the held handler reached its hold before the peer went away"
    );
    drop(socket);
    fixture.gate.release();
    assert_ownership_released(
        "a disconnect releases everything the session owned",
        fixture,
        controller,
        before,
        released,
        &Ownership {
            reads: true,
            settles: Settles::Once,
        },
    );
    let label = "a truncated payload is unreadable";
    let observed = awaited(&fixture.mapped, label);
    assert_eq!(
        (observed.origin, observed.kind),
        (LIVE, RejectionKind::BodyUnreadable),
        "{label}: classified once, with nobody left to answer: {observed:?}"
    );
}

/// Wait for exactly one refusal to be recorded, and report it.
///
/// The peer whose request this belongs to is gone, so nothing on the wire says
/// when its answer was built. The journal is the only arrival left to wait for.
fn awaited(mapped: &Journal, label: &str) -> Observed {
    let recorded = wire::poll_value(BOUND, || {
        let observed = drain(mapped);
        (!observed.is_empty()).then_some(observed)
    })
    .unwrap_or_else(|| panic!("{label}: nothing was classified"));
    let mut recorded = recorded.into_vec();
    assert_eq!(recorded.len(), 1, "{label}: classified once: {recorded:?}");
    recorded.remove(0)
}

/// Assert a reset HTTP/2 stream releases the session it left behind.
///
/// The handler is a reading one rather than a held one, and that is the whole
/// design of the row. A reset is delivered to whatever is polling the stream, so
/// a session parked on something else would be proving the transport's timing
/// rather than the session's ownership. This one is mid-field and asking for its
/// next chunk when the stream disappears underneath it.
async fn assert_reset_releases(
    addr: SocketAddr,
    fixture: &Fixture,
    controller: &LifecycleController,
    released: usize,
) {
    let before = controller.multipart_observed();
    let accepted = before.commands_accepted();
    let body = held_body();
    let mut client = PersistentH2Client::connect(addr, BOUND).await;
    let mut upload = open_upload(&mut client, "/read").await;
    let mut offered = 0;
    // Two commands: the field, and the chunk read the reset interrupts.
    assert!(
        offer_frames_until(&mut upload, &body, &mut offered, || controller
            .multipart_observed()
            .commands_accepted()
            >= accepted + 2)
        .await,
        "the reading handler was mid-field when its stream was reset"
    );
    upload.reset();
    drop(upload);
    client.close().await;
    assert_ownership_released(
        "a stream reset releases everything the session owned",
        fixture,
        controller,
        before,
        released,
        &Ownership {
            reads: true,
            settles: Settles::Either,
        },
    );

    // Either the transport tore the request down before anything could answer
    // it, or the read failed on a stream that was already gone. Both are the
    // same claim about ownership; only the second leaves a classification.
    let label = "a reset stream";
    let recorded = settled_classification(&fixture.mapped).await;
    assert!(
        recorded.len() <= 1,
        "{label} is classified at most once: {recorded:?}"
    );
    assert!(
        recorded
            .iter()
            .all(|observed| (observed.origin, observed.kind)
                == (LIVE, RejectionKind::BodyUnreadable)),
        "{label} is classified by the listener's own mapper as unreadable if it is classified at all: {recorded:?}"
    );
}

/// What the journal holds once it has stopped changing, for a row that may or
/// may not leave one classification.
///
/// Neither outcome can be polled for alone: an entry is an arrival, and an empty
/// journal read the instant the row ends is empty for a classification that is
/// merely late as well as for one that will never come — which passes the row
/// whatever the answer was classified as. So the entry is waited for, and
/// emptiness is accepted only after the window it had to appear in has passed.
/// This is not a rendezvous: everything the row asserts on has already been
/// waited for by the time it is called.
async fn settled_classification(mapped: &Journal) -> Box<[Observed]> {
    let mapped = Arc::clone(mapped);
    tokio::task::spawn_blocking(move || {
        wire::poll_value(QUIET, || {
            let observed = drain(&mapped);
            (!observed.is_empty()).then_some(observed)
        })
        .unwrap_or_default()
    })
    .await
    .expect("the reset row's classification window was waited out")
}

/// Assert one cancellation row answers as its terminal states and releases
/// everything.
async fn assert_cancellation_row(
    after: Cancelled,
    addr: SocketAddr,
    fixture: &Fixture,
    controller: &Arc<LifecycleController>,
    released: usize,
) {
    let (path, kind) = match after {
        Cancelled::Report => ("/cancel-error", RejectionKind::Application),
        Cancelled::Retry => ("/cancel-retry", RejectionKind::Multipart),
    };
    let before = controller.multipart_observed();
    let accepted = before.commands_accepted();
    let mut socket = send_prefix(addr, wire::CLOSE_AFTER_RESPONSE, path, &held_body());
    // Three commands: the field, the chunk that proves ingress is live, and the
    // one the handler drops where it waits.
    let observing = Arc::clone(controller);
    assert!(
        arrived(BOUND, move || {
            observing.multipart_observed().commands_accepted() >= accepted + 3
        })
        .await,
        "{path}: the read the handler cancels was accepted first"
    );
    fixture.gate.release();

    let answered = wire::read_http_response_bounded(&mut socket)
        .unwrap_or_else(|error| panic!("{path}: no response: {error}"));
    assert_eq!(
        answered.status, 400,
        "{path}: a lost accepted read cannot answer with a complete body"
    );
    assert_classified_once(&fixture.mapped, path, kind);
    assert_ownership_released(
        path,
        fixture,
        controller,
        before,
        released,
        &Ownership {
            reads: true,
            settles: Settles::Once,
        },
    );
    wire::assert_connection_closed(&mut socket, path);
}

/// The payload the shutdown row uploads.
///
/// Small enough that the peer can hand the whole thing to the transport while
/// the handler is holding, so the only thing the stop is waiting on is the
/// handler itself.
const SHUTDOWN_BYTES: usize = 128 * 1024;

/// Assert a stop found a session in flight, drained it, and released what it
/// owned.
///
/// Its own listener, because the claim ends the server: a row sharing a fixture
/// with the rows after it would be deciding their transport for them.
async fn assert_shutdown_drains_held_session() {
    let fixture = Fixture::new();
    let port = wire::reserve_observed();
    let controller = port.controller();
    let server = port.serve(live_router(&fixture));
    let addr = server.addr();
    let before = controller.multipart_observed();
    let body = multipart_body(BOUNDARY, &[Field::bytes("upload", &[5u8; SHUTDOWN_BYTES])])
        .into_boxed_slice();

    let reading = tokio::task::spawn_blocking(move || upload(addr, "/hold", &body));
    let gate = Arc::clone(&fixture.gate);
    assert!(
        arrived(BOUND, move || gate.reached() == 1).await,
        "the held handler reached its hold before the server was stopped"
    );

    // The quiet window below is the claim that the stop has not returned, and it
    // is only a claim about the stop once the stop has begun: a blocking task the
    // pool has not picked up yet is unfinished for a reason that has nothing to
    // do with the session in flight. So the stop announces its own entry and the
    // window opens on that.
    let entered = Arc::new(AtomicBool::new(false));
    let entering = Arc::clone(&entered);
    let stopping = tokio::task::spawn_blocking(move || {
        entering.store(true, Ordering::SeqCst);
        server.shutdown_bounded(BOUND)
    });
    assert!(
        arrived(BOUND, move || entered.load(Ordering::SeqCst)).await,
        "the graceful stop was entered before its quiet window opened"
    );
    assert!(
        !wire::poll_until(QUIET, || stopping.is_finished()),
        "a graceful stop waits for the session it found in flight"
    );

    fixture.gate.release();
    let answered = reading.await.expect("the held request finished");
    assert_eq!(
        (answered.status, String::from_utf8_lossy(&answered.body)),
        (200, format!("held {SHUTDOWN_BYTES}").into()),
        "the drained session answered its peer before the server stopped"
    );
    stopping
        .await
        .expect("the shutdown task finished")
        .expect("the fixture stopped once its session had drained");
    assert_ownership_released(
        "a drained shutdown releases everything the session owned",
        &fixture,
        &controller,
        before,
        1,
        &Ownership {
            reads: true,
            settles: Settles::Once,
        },
    );
}

/// Assert a request future dropped mid-handler takes the whole session with it.
///
/// This is the handler's own cancellation, and it is not the reset row above.
/// There the handler is inside a read the transport can fail, so the session
/// can end either way; here the handler is parked on a hold nothing will ever
/// release, and its own read is not in flight. So the only thing that can end
/// anything this request owns is the request future being dropped underneath
/// it, which is what Hyper does to a stream its peer resets.
///
/// That is also what makes this the one row that can state the negative: a
/// driver, a handler, or a permit that some task had taken over would still be
/// parked at that hold when this returns, because nothing else can wake it. The
/// permit comes back, the handler's own witness reports it was dropped where it
/// stood, and no driver ever settled — so nothing was left running behind the
/// cancelled request.
async fn assert_handler_cancellation_releases(
    addr: SocketAddr,
    fixture: &Fixture,
    controller: &LifecycleController,
    released: usize,
) {
    let before = controller.multipart_observed();
    let (dropped, resumed) = (fixture.held.dropped(), fixture.held.resumed());
    let held = fixture.gate.reached();
    let body = held_body();
    let mut client = PersistentH2Client::connect(addr, BOUND).await;
    let mut upload = open_upload(&mut client, "/hold").await;
    let mut offered = 0;
    assert!(
        offer_frames_until(&mut upload, &body, &mut offered, || fixture.gate.reached()
            == held + 1)
        .await,
        "the handler took its first chunk and holds: {offered} bytes granted"
    );

    upload.reset();
    drop(upload);
    client.close().await;
    let label = "a cancelled handler releases everything its request owned";
    assert_ownership_released(
        label,
        fixture,
        controller,
        before,
        released,
        &Ownership {
            reads: true,
            settles: Settles::Never,
        },
    );
    let witness = Arc::clone(&fixture.held);
    assert!(
        arrived(BOUND, move || witness.dropped() == dropped + 1).await,
        "{label}: the handler future was dropped, {} of {} so far",
        fixture.held.dropped(),
        dropped + 1
    );
    assert_eq!(
        fixture.held.resumed(),
        resumed,
        "{label}: the cancelled handler never passed the hold it was dropped at"
    );
    assert!(
        drain(&fixture.mapped).is_empty(),
        "{label}: a request nothing can answer classifies nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_reset_shutdown_and_cancellation_release_multipart_ownership() {
    let fixture = Fixture::new();
    let port = wire::reserve_observed();
    let controller = port.controller();
    let server = port.serve(live_router(&fixture));
    let addr = server.addr();

    let mut released = 0;
    for row in TERMINALS {
        released += 1;
        assert_terminal_row(row, addr, &fixture, &controller, released);
    }
    released += 1;
    assert_disconnect_releases(addr, &fixture, &controller, released).await;
    released += 1;
    assert_reset_releases(addr, &fixture, &controller, released).await;
    for after in [Cancelled::Report, Cancelled::Retry] {
        released += 1;
        assert_cancellation_row(after, addr, &fixture, &controller, released).await;
    }
    // Last of the held rows, and it stays last: its handler is dropped at a hold
    // no later row may release.
    released += 1;
    assert_handler_cancellation_releases(addr, &fixture, &controller, released).await;

    // Every one of those ended without taking the listener with it.
    let answered = upload(addr, "/read", &valid_body());
    assert_eq!(
        answered.status, 200,
        "the listener still answers after every terminal above"
    );
    released += 1;
    wire::assert_released(
        &fixture.released,
        released,
        "one permit released per request, and no session left holding one",
    );
    server.shutdown_bounded(BOUND).expect("the fixture stopped");

    assert_shutdown_drains_held_session().await;
}

/// Which transport one escaped-handle row travels on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Transport {
    Http1,
    Http2,
}

/// Send one escaping row's request and hand back the answer when it arrives.
///
/// The exchange runs in a task of its own because the case has to make its
/// observations while the request is paused inside the response-selection
/// checkpoint.
fn send_escaping(
    transport: Transport,
    addr: SocketAddr,
    path: &'static str,
) -> tokio::task::JoinHandle<wire::HttpResponse> {
    let body = valid_body();
    match transport {
        Transport::Http1 => tokio::task::spawn_blocking(move || upload(addr, path, &body)),
        Transport::Http2 => tokio::spawn(async move {
            let mut client = PersistentH2Client::connect(addr, BOUND).await;
            let declared = declared();
            let answered = client
                .send_complete(
                    "POST",
                    path,
                    HOST,
                    &[("content-type", declared.as_ref())],
                    &body,
                )
                .await;
            client.close().await;
            answered
        }),
    }
}

/// One escaped-handle row: where the handler leaves its handle, and what
/// carries the request it left it on.
#[derive(Clone, Copy)]
struct Escaping {
    transport: Transport,
    escape: Escape,
    path: &'static str,
    addr: SocketAddr,
}

/// Drive one escaped-handle row over a real transport and assert what its
/// session had already released when its answer was selected.
async fn assert_escape_row(
    row: &Escaping,
    fixture: &Fixture,
    controller: &Arc<LifecycleController>,
    released: usize,
) {
    let &Escaping {
        transport,
        escape,
        path,
        addr,
    } = row;
    let checkpoint = LifecycleCheckpoint::BeforeMultipartResponseSelection;
    let before = controller.multipart_observed();
    controller
        .pause_once(checkpoint)
        .expect("the response-selection checkpoint armed");
    let sending = send_escaping(transport, addr, path);
    wire::wait_until_paused_bounded(controller, checkpoint, path).await;

    let observed = controller.multipart_observed();
    assert_eq!(
        (observed.revocations(), observed.drivers_terminated()),
        (before.revocations() + 1, before.drivers_terminated() + 1),
        "{path}: revocation and driver termination precede response selection: {observed:?}"
    );
    assert_eq!(
        fixture.released.load(Ordering::SeqCst),
        released,
        "{path}: the admitted permit is released before the response is selected"
    );

    assert_escaped_inert(
        &fixture.escapes,
        escape,
        observed.body_frames_polled(),
        controller,
        path,
        BOUND,
    )
    .await;
    controller
        .release(checkpoint)
        .expect("the checkpoint released");

    let answered = sending.await.expect("the escaping request finished");
    assert_eq!(
        answered.status, 400,
        "{path}: a handler that handed its stream away read nothing"
    );
    let mapped = only(&fixture.mapped, path);
    assert_eq!(
        (mapped.origin, mapped.kind),
        (LIVE, RejectionKind::Multipart),
        "{path}: an incomplete body is classified once: {mapped:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn escaped_stream_handle_is_inert_after_live_handler_completion() {
    let fixture = Fixture::new();
    let port = wire::reserve_observed();
    let controller = port.controller();
    let server = port.serve(live_router(&fixture));
    let addr = server.addr();

    let rows = [
        Escaping {
            transport: Transport::Http1,
            escape: Escape::Channel,
            path: "/channel",
            addr,
        },
        Escaping {
            transport: Transport::Http2,
            escape: Escape::Task,
            path: "/task",
            addr,
        },
    ];
    for (index, row) in rows.iter().enumerate() {
        assert_escape_row(row, &fixture, &controller, index + 1).await;
    }

    server.shutdown_bounded(BOUND).expect("the fixture stopped");
}
