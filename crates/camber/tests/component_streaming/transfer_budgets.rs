//! 11.T1, 11.T2, and 11.T3: one owner per streaming direction.
//!
//! Every row here enters through the public surface a service configures — a
//! router's transfer policies, `StreamResponse::with_budget`, an SSE
//! registration's budget, and a served streaming multipart route — and reads
//! back what the production owners published about themselves. Nothing in this
//! file selects a terminal, admits a byte, polls a source, or releases a
//! producer.

use crate::http as http_support;
use crate::stream_support::{Streamed, open_streaming, read_streamed};

use camber::http::mock::{
    InboundTerminal, LifecycleCheckpoint, LifecycleController, TransferObservation,
};
use camber::http::{
    Method, MultipartLimits, MultipartStream, Request, RequestBudget, Response, Router,
    StreamResponse, TransferBudget,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long any row waits for a production event it has staged.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a row waits for the served fixture to stop.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// The aggregate deadline the shutdown row's server names.
const SHUTDOWN_DEADLINE: Duration = Duration::from_millis(200);
/// The quiet interval a row that means to cross one configures.
const CROSSED_IDLE: Duration = Duration::from_millis(150);
/// The lifetime a row that means to cross one configures.
const CROSSED_TOTAL: Duration = Duration::from_millis(400);
/// A bound no row is allowed to reach.
const UNREACHED: Duration = Duration::from_secs(30);
/// How long an SSE producer stays quiet after its one event.
///
/// Longer than the interval every SSE row crosses, and short enough that the
/// blocking producer this row leaves behind releases its thread inside the
/// fixture's own teardown rather than holding it for the whole unreachable bound.
const SSE_QUIET: Duration = Duration::from_secs(1);
/// The payload maximum the byte rows are bounded by.
const MAX_BYTES: usize = 24;
/// One frame of the byte rows' payload. Three of them reach the maximum exactly.
const FRAME: &[u8] = b"12345678";
/// How long a paced producer waits between frames.
///
/// Well under every crossed bound, and long enough that each frame reaches the
/// peer as its own write: a producer that filled the channel in one turn would
/// leave Hyper holding every frame in one unflushed buffer, and a body that then
/// ended would take the whole of it with the transport.
const PACE: Duration = Duration::from_millis(20);

/// The multipart boundary the upload rows declare.
const BOUNDARY: &str = "CamberTransferBnd";

/// Wait until the production owner has been held at `checkpoint`.
async fn wait_paused(controller: &LifecycleController, checkpoint: LifecycleCheckpoint, row: &str) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap_or_else(|_| panic!("{row}: {checkpoint:?} was never reached"))
        .unwrap_or_else(|error| panic!("{row}: waiting for {checkpoint:?} failed: {error}"));
}

/// Wait until this listener's transfers satisfy `settled`, and report them.
async fn awaited(
    controller: &LifecycleController,
    row: &str,
    settled: impl Fn(&TransferObservation) -> bool,
) -> TransferObservation {
    let deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
    loop {
        let observed = controller.transfers_observed();
        if settled(&observed) {
            return observed;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{row}: the transfer never settled: {observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A producer that sends `frames` copies of [`FRAME`] and then holds.
///
/// It holds rather than returning, so a row asserting on a byte or a deadline
/// terminal is asserting on the owner's decision and not on a producer that
/// happened to finish first. The producer's own end is a separate row.
fn holding_stream(frames: usize, budget: TransferBudget) -> Router {
    let mut router = Router::new();
    router.get_stream("/stream", move |_req: &Request| {
        let budget = budget;
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(200, 4, budget)
                .expect("a positive stream capacity is accepted");
            tokio::spawn(async move {
                for _ in 0..frames {
                    if sender.send(FRAME).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(PACE).await;
                }
                // Held open: the owner's terminal, not the producer's end, is
                // what every row below reads.
                std::future::pending::<()>().await;
            });
            response
        })
    });
    router
}

/// 11.T1
///
/// Bytes, quiet interval, and lifetime are counted per direction, and the two
/// directions of one operation never write to each other's state. Each row
/// configures one dimension to be crossed and the others out of reach, then
/// reads the terminal the production owner fixed and what it published about the
/// frames it polled and the bytes it admitted.
#[test]
fn generic_transfer_directions_account_bytes_idle_and_total_independently() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_exact_limit_row().await;
                assert_crossing_row().await;
                assert_download_idle_row().await;
                assert_download_total_row().await;
                assert_empty_frame_row().await;
                assert_sink_backpressure_row().await;
                assert_narrowing_rows().await;
                assert_sse_rows().await;
                assert_upload_idle_row().await;
                assert_upload_trailer_row().await;
            });
        })
        .expect("the transfer accounting runtime ran");
}

/// A payload that reaches the maximum exactly is delivered in full.
async fn assert_exact_limit_row() {
    let row = "the exact maximum";
    let budget = TransferBudget::bounded(MAX_BYTES, UNREACHED, UNREACHED)
        .expect("the exact-limit budget is accepted");
    let port = http_support::reserve_observed();
    let server = port.serve(ending_stream(MAX_BYTES / FRAME.len(), budget));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        delivered,
        Streamed {
            status: 200,
            bytes: MAX_BYTES,
            complete: true,
        },
        "{row}: every admitted byte reaches the peer and the body ends cleanly"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::ResponseHead),
        "{row}: the source's own end is the terminal: {observed:?}"
    );
    assert_eq!(
        observed.download.admitted_bytes, MAX_BYTES,
        "{row}: the maximum is admitted, not refused"
    );
    assert_eq!(
        observed.download.crossings_released, 0,
        "{row}: nothing was released instead of delivered"
    );
    assert_eq!(
        observed.upload,
        Default::default(),
        "{row}: a download writes nothing to the upload's record: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// A producer that sends `frames` copies of [`FRAME`] and then ends.
fn ending_stream(frames: usize, budget: TransferBudget) -> Router {
    let mut router = Router::new();
    router.get_stream("/stream", move |_req: &Request| {
        let budget = budget;
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(200, 4, budget)
                .expect("a positive stream capacity is accepted");
            tokio::spawn(async move {
                for _ in 0..frames {
                    if sender.send(FRAME).await.is_err() {
                        return;
                    }
                }
            });
            response
        })
    });
    router
}

/// The frame that crosses the maximum is released, and the committed status
/// stands.
async fn assert_crossing_row() {
    let row = "the crossing frame";
    let budget = TransferBudget::bounded(MAX_BYTES, UNREACHED, UNREACHED)
        .expect("the crossing budget is accepted");
    let port = http_support::reserve_observed();
    let server = port.serve(holding_stream(MAX_BYTES / FRAME.len() + 1, budget));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        delivered,
        Streamed {
            status: 200,
            bytes: MAX_BYTES,
            complete: false,
        },
        "{row}: the peer keeps the committed status and never receives the crossing bytes"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::TransferBytes),
        "{row}: the byte maximum is the terminal: {observed:?}"
    );
    assert_eq!(
        observed.download.admitted_bytes, MAX_BYTES,
        "{row}: no byte past the maximum is admitted"
    );
    assert_eq!(
        observed.download.crossings_released, 1,
        "{row}: the crossing frame's backing is released rather than delivered"
    );
    assert_eq!(
        observed.download.terminals, 1,
        "{row}: exactly one terminal is fixed"
    );
    drop(peer);
    stop(server, row);
}

/// A source that stops producing crosses the quiet interval, and only that.
async fn assert_download_idle_row() {
    let row = "the download quiet interval";
    let budget = TransferBudget::unbounded()
        .with_idle(CROSSED_IDLE)
        .expect("the idle budget is accepted");
    let port = http_support::reserve_observed();
    let server = port.serve(holding_stream(1, budget));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        (delivered.status, delivered.bytes, delivered.complete),
        (200, FRAME.len(), false),
        "{row}: the frame that arrived is delivered and the stream then ends"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::TransferIdle),
        "{row}: the quiet interval is the terminal: {observed:?}"
    );
    assert_eq!(
        observed.download.idle,
        Some(CROSSED_IDLE),
        "{row}: the frozen interval is the one this stream named"
    );
    assert_eq!(
        observed.upload,
        Default::default(),
        "{row}: an idle download leaves the upload record untouched"
    );
    drop(peer);
    stop(server, row);
}

/// A source that keeps producing still spends its lifetime.
///
/// Every frame restarts the quiet interval, and the interval configured here is
/// longer than the whole lifetime, so an owner that reset its lifetime on a
/// delivered frame would never reach a terminal at all.
async fn assert_download_total_row() {
    let row = "the download lifetime";
    let budget = TransferBudget::unbounded()
        .with_total(CROSSED_TOTAL)
        .expect("the total budget is accepted");
    let port = http_support::reserve_observed();
    let server = port.serve(paced_stream(budget));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        (delivered.status, delivered.complete),
        (200, false),
        "{row}: the committed status stands and the body is cut rather than ended"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::TransferTotal),
        "{row}: a lifetime a delivered frame cannot restart is the terminal: {observed:?}"
    );
    assert!(
        observed.download.frames_polled > 1,
        "{row}: more than one frame crossed before the lifetime expired: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// A producer that keeps sending a frame every interval, forever.
fn paced_stream(budget: TransferBudget) -> Router {
    let mut router = Router::new();
    router.get_stream("/stream", move |_req: &Request| {
        let budget = budget;
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(200, 4, budget)
                .expect("a positive stream capacity is accepted");
            tokio::spawn(async move {
                loop {
                    if sender.send(FRAME).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(CROSSED_TOTAL / 8).await;
                }
            });
            response
        })
    });
    router
}

/// A frame that carried no payload is polled, counted, and restarts nothing.
async fn assert_empty_frame_row() {
    let row = "an empty frame";
    let budget = TransferBudget::bounded(MAX_BYTES, CROSSED_IDLE, UNREACHED)
        .expect("the empty-frame budget is accepted");
    let port = http_support::reserve_observed();
    let server = port.serve(empty_frame_stream(budget));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        (delivered.status, delivered.bytes),
        (200, 0),
        "{row}: no payload reaches the peer"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::TransferIdle),
        "{row}: an empty frame does not restart the quiet interval: {observed:?}"
    );
    assert!(
        observed.download.frames_polled >= 1,
        "{row}: the empty frame was polled: {observed:?}"
    );
    assert_eq!(
        observed.download.admitted_bytes, 0,
        "{row}: an empty frame costs no payload bytes"
    );
    drop(peer);
    stop(server, row);
}

/// A producer whose only frame carries nothing, and which then holds.
fn empty_frame_stream(budget: TransferBudget) -> Router {
    let mut router = Router::new();
    router.get_stream("/stream", move |_req: &Request| {
        let budget = budget;
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(200, 4, budget)
                .expect("a positive stream capacity is accepted");
            tokio::spawn(async move {
                if sender.send(&b""[..]).await.is_err() {
                    return;
                }
                std::future::pending::<()>().await;
            });
            response
        })
    });
    router
}

/// Time the sink spent not taking frames is charged to the lifetime and not to
/// the source's quiet interval.
///
/// The owner is held at its own source checkpoint for longer than both bounds
/// while its producer already has a frame waiting. That is what a sink which is
/// not taking frames looks like from inside the owner: nothing is read, and
/// nothing can be, until the sink comes back. On release the waiting frame
/// crosses in that same turn and restarts the quiet interval, so the interval is
/// not ready — and the lifetime, which is absolute, spent the whole wait and is.
/// The declared order puts the interval above the lifetime, so an owner that
/// charged this wait to the source would answer with the interval instead.
async fn assert_sink_backpressure_row() {
    assert_held_turn(
        "sink backpressure",
        TransferBudget::bounded(usize::MAX, CROSSED_IDLE, CROSSED_TOTAL)
            .expect("the backpressure budget is accepted"),
        1,
        None,
        InboundTerminal::TransferTotal,
    )
    .await;
}

/// Hold one download owner's turn until every staged source is ready, and read
/// back which of them the declared order named.
///
/// The hold is the owner's own pre-source checkpoint, so the turn that decides
/// sees each staged source in one reading. `frames` is what the producer queues,
/// and `armed_after` delays the hold until that many payload bytes have already
/// been admitted — which is how a row stages the frame that crosses the maximum
/// as the one read inside the held turn.
async fn assert_held_turn(
    row: &str,
    budget: TransferBudget,
    frames: usize,
    armed_after: Option<usize>,
    expected: InboundTerminal,
) {
    let port = http_support::reserve_observed();
    let server = port.serve(holding_stream(frames, budget));
    let addr = server.addr();
    let held = LifecycleCheckpoint::BeforeTransferSourcePoll;

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    if let Some(admitted) = armed_after {
        awaited(server.controller(), row, |observed| {
            observed.download.admitted_bytes >= admitted
        })
        .await;
    }
    server
        .controller()
        .pause_once(held)
        .expect("arm the transfer's own source checkpoint");
    wait_paused(server.controller(), held, row).await;
    // Both deadlines expire while the turn is held, and whatever the producer
    // queued behind it is waiting when it resumes.
    tokio::time::sleep(CROSSED_TOTAL + CROSSED_IDLE).await;
    server
        .controller()
        .release(held)
        .expect("release the transfer's source checkpoint");

    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        (delivered.status, delivered.complete),
        (200, false),
        "{row}: the committed status stands and the body is cut rather than ended"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(expected),
        "{row}: the declared order named this cause: {observed:?}"
    );
    assert_eq!(
        observed.download.terminals, 1,
        "{row}: one cause, fixed once"
    );
    drop(peer);
    stop(server, row);
}

/// An inner unbounded dimension inherits the outer bound, and an inner finite
/// one narrows it.
async fn assert_narrowing_rows() {
    let outer = TransferBudget::bounded(MAX_BYTES * 4, UNREACHED, UNREACHED)
        .expect("the router's download policy is accepted");

    assert_frozen_download(
        "an inner unbounded policy",
        outer,
        TransferBudget::unbounded(),
        outer,
    )
    .await;
    let inner = TransferBudget::unbounded()
        .with_max_bytes(MAX_BYTES)
        .expect("the stream's own maximum is accepted");
    assert_frozen_download(
        "an inner finite maximum",
        outer,
        inner,
        TransferBudget::bounded(MAX_BYTES, UNREACHED, UNREACHED)
            .expect("the narrowed policy is representable"),
    )
    .await;
}

/// Serve one stream under a router policy and read back the policy it froze.
async fn assert_frozen_download(
    row: &str,
    outer: TransferBudget,
    inner: TransferBudget,
    expected: TransferBudget,
) {
    let port = http_support::reserve_observed();
    let server = port.serve(ending_stream(1, inner).download_budget(outer));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(delivered.status, 200, "{row}: the stream was served");
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.max_bytes.is_some()
    })
    .await;
    assert_eq!(
        (
            observed.download.max_bytes,
            observed.download.idle,
            observed.download.total
        ),
        (expected.max_bytes(), expected.idle(), expected.total()),
        "{row}: the frozen policy is the narrowed one: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// An SSE registration is bounded by the budget it names, and an SSE
/// registration that names none inherits its router's.
async fn assert_sse_rows() {
    let row = "an SSE registration's own interval";
    let port = http_support::reserve_observed();
    let mut router = Router::new();
    router.get_sse_with_budget(
        "/events",
        TransferBudget::unbounded()
            .with_idle(CROSSED_IDLE)
            .expect("the SSE interval is accepted"),
        |_req: &Request, writer: &mut camber::http::SseWriter| {
            writer.event("tick", "1")?;
            // The producer stops publishing while its feed stays open, which is
            // the condition the interval exists to end.
            std::thread::sleep(SSE_QUIET);
            Ok(())
        },
    );
    let server = port.serve(router);
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/events", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(delivered.status, 200, "{row}: the feed committed its 200");
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        (observed.download.terminal, observed.download.idle),
        (Some(InboundTerminal::TransferIdle), Some(CROSSED_IDLE)),
        "{row}: the registration's interval ended the feed: {observed:?}"
    );
    drop(peer);
    stop(server, row);

    let row = "an SSE registration with no policy of its own";
    let inherited = TransferBudget::unbounded()
        .with_idle(CROSSED_IDLE)
        .expect("the router's download interval is accepted");
    let port = http_support::reserve_observed();
    let mut router = Router::new();
    router.get_sse(
        "/events",
        |_req: &Request, writer: &mut camber::http::SseWriter| {
            writer.event("tick", "1")?;
            std::thread::sleep(SSE_QUIET);
            Ok(())
        },
    );
    let server = port.serve(router.download_budget(inherited));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/events", EVENT_TIMEOUT).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(delivered.status, 200, "{row}: the feed committed its 200");
    let observed = awaited(server.controller(), row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        (observed.download.terminal, observed.download.idle),
        (Some(InboundTerminal::TransferIdle), Some(CROSSED_IDLE)),
        "{row}: the router's interval reached the feed that named none: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// The upload's quiet interval ends an upload, and writes nothing a download
/// reads.
async fn assert_upload_idle_row() {
    let row = "the upload quiet interval";
    let port = http_support::reserve_observed();
    let upload = TransferBudget::unbounded()
        .with_idle(CROSSED_IDLE)
        .expect("the upload interval is accepted");
    let download = TransferBudget::bounded(MAX_BYTES, UNREACHED, UNREACHED)
        .expect("the download policy is accepted");
    let server = port.serve(
        multipart_router()
            .upload_budget(upload)
            .download_budget(download),
    );
    let addr = server.addr();

    let mut peer = stalled_upload(addr).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        delivered.status, 408,
        "{row}: the crossed interval is mapped once, before any head committed"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.upload.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.upload.terminal,
        Some(InboundTerminal::TransferIdle),
        "{row}: the upload's own interval is the terminal: {observed:?}"
    );
    assert_eq!(
        observed.upload.idle,
        Some(CROSSED_IDLE),
        "{row}: the upload froze the upload policy, not the download's"
    );
    assert_eq!(
        observed.upload.max_bytes, None,
        "{row}: route-aware admission stays the only payload-byte authority"
    );
    assert_eq!(
        observed.download.terminal, None,
        "{row}: an ended upload fixes no download terminal: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// A trailer costs no payload bytes and ends nothing.
async fn assert_upload_trailer_row() {
    let row = "a trailered upload";
    let port = http_support::reserve_observed();
    let server = port.serve(
        multipart_router().upload_budget(
            TransferBudget::unbounded()
                .with_idle(UNREACHED)
                .expect("the upload interval is accepted"),
        ),
    );
    let addr = server.addr();
    let body = multipart_body();

    let mut peer = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect the trailered upload");
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: transfer.test\r\nContent-Type: multipart/form-data; boundary={BOUNDARY}\r\nTransfer-Encoding: chunked\r\n\r\n"
    );
    peer.write_all(head.as_bytes())
        .await
        .expect("write the trailered upload head");
    peer.write_all(format!("{:x}\r\n", body.len()).as_bytes())
        .await
        .expect("write the chunk size");
    peer.write_all(&body).await.expect("write the payload");
    peer.write_all(b"\r\n0\r\nX-Camber-Trailer: seen\r\n\r\n")
        .await
        .expect("write the terminal chunk and its trailer");
    peer.flush().await.expect("flush the trailered upload");

    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        delivered.status, 200,
        "{row}: a trailer changes no multipart result"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.upload.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.upload.terminal,
        Some(InboundTerminal::ResponseHead),
        "{row}: the payload's own end is the terminal: {observed:?}"
    );
    assert_eq!(
        observed.upload.admitted_bytes,
        body.len(),
        "{row}: the trailer adds no payload bytes: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// One multipart route that reads its whole payload and reports what it read.
fn multipart_router() -> Router {
    let mut router = Router::new();
    router.multipart(
        Method::Post,
        "/upload",
        MultipartLimits::default(),
        |_req: &Request, mut stream: MultipartStream| async move {
            let mut read = 0;
            while let Some(mut field) = stream.next_field().await? {
                while let Some(chunk) = field.next_chunk().await? {
                    read += chunk.len();
                }
            }
            Response::text(200, &format!("read {read}"))
        },
    );
    router
}

/// The multipart payload both upload rows send.
fn multipart_body() -> Box<[u8]> {
    format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"upload\"\r\n\r\n{}\r\n--{BOUNDARY}--\r\n",
        String::from_utf8_lossy(FRAME)
    )
    .into_bytes()
    .into_boxed_slice()
}

/// Open one multipart upload, send its opening bytes, and then stop sending.
///
/// The declared length is longer than what is written, so the payload is neither
/// complete nor failed: it is quiet, which is the condition the upload's own
/// interval exists to end.
async fn stalled_upload(addr: SocketAddr) -> tokio::net::TcpStream {
    let body = multipart_body();
    let mut peer = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect the stalled upload");
    let head = format!(
        "POST /upload HTTP/1.1\r\nHost: transfer.test\r\nContent-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\n\r\n",
        body.len() + FRAME.len()
    );
    peer.write_all(head.as_bytes())
        .await
        .expect("write the stalled upload head");
    peer.write_all(&body)
        .await
        .expect("write the stalled upload's opening bytes");
    peer.flush().await.expect("flush the stalled upload");
    peer
}

/// Stop one served fixture and report a teardown that did not finish.
fn stop(server: http_support::ObservedServer, row: &str) {
    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
}

/// 11.T2
///
/// A terminal is fixed once, and nothing is polled after it. Each row witnesses
/// one frame staged behind the terminal it selects: the frame is queued while the
/// production owner is held, and the row then reads the production frame-poll
/// count on both sides of the selection. An owner missing its latch or its poll
/// guard reads that witnessed frame and fails the row it is staged in.
#[test]
fn terminal_transfer_polls_no_later_frame() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_byte_terminal_polls_no_later_frame().await;
                assert_deadline_terminal_polls_no_later_frame(
                    "an idle terminal",
                    TransferBudget::unbounded()
                        .with_idle(CROSSED_IDLE)
                        .expect("the idle budget is accepted"),
                    InboundTerminal::TransferIdle,
                )
                .await;
                assert_deadline_terminal_polls_no_later_frame(
                    "a lifetime terminal",
                    TransferBudget::unbounded()
                        .with_total(CROSSED_TOTAL)
                        .expect("the total budget is accepted"),
                    InboundTerminal::TransferTotal,
                )
                .await;
                assert_released_on_disconnect().await;
                assert_released_on_shutdown().await;
            });
        })
        .expect("the terminal-poll runtime ran");
}

/// A producer whose later frames are released by the row that stages them.
///
/// `before` frames are paced out so the peer receives each as its own write.
/// After that the producer waits, and the row lets it queue `after` frames at the
/// exact moment it wants them witnessed. Its `sent` counter is the row's proof
/// that the frames it asserts were never polled really existed.
struct Witnessed {
    gate: Arc<tokio::sync::Notify>,
    sent: Arc<AtomicUsize>,
}

impl Witnessed {
    fn new() -> Self {
        Self {
            gate: Arc::new(tokio::sync::Notify::new()),
            sent: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Let the producer queue the frames it was holding back.
    fn release(&self) {
        self.gate.notify_one();
    }

    /// How many withheld frames the producer has queued.
    fn sent(&self) -> usize {
        self.sent.load(Ordering::Acquire)
    }

    /// One route serving this producer.
    fn router(&self, before: usize, after: usize, budget: TransferBudget) -> Router {
        let mut router = Router::new();
        let gate = Arc::clone(&self.gate);
        let sent = Arc::clone(&self.sent);
        router.get_stream("/stream", move |_req: &Request| {
            let budget = budget;
            let gate = Arc::clone(&gate);
            let sent = Arc::clone(&sent);
            Box::pin(async move {
                let (response, sender) = StreamResponse::with_budget(200, 8, budget)
                    .expect("a positive stream capacity is accepted");
                tokio::spawn(async move {
                    for _ in 0..before {
                        if sender.send(FRAME).await.is_err() {
                            return;
                        }
                        tokio::time::sleep(PACE).await;
                    }
                    gate.notified().await;
                    for _ in 0..after {
                        if sender.send(FRAME).await.is_err() {
                            return;
                        }
                        sent.fetch_add(1, Ordering::Release);
                    }
                    std::future::pending::<()>().await;
                });
                response
            })
        });
        router
    }
}

/// The byte row: the crossing frame and one behind it are queued together.
///
/// The owner is held at its source checkpoint once the maximum has already been
/// reached, and both remaining frames are queued while it waits. The released
/// turn reads the crossing frame, releases it, and fixes the terminal — and the
/// frame behind it is still in the channel, unpolled.
async fn assert_byte_terminal_polls_no_later_frame() {
    let row = "a byte terminal";
    let witnessed = Witnessed::new();
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let server = port.serve(
        witnessed.router(
            MAX_BYTES / FRAME.len(),
            2,
            TransferBudget::bounded(MAX_BYTES, UNREACHED, UNREACHED)
                .expect("the byte budget is accepted"),
        ),
    );
    let addr = server.addr();
    let held = LifecycleCheckpoint::BeforeTransferSourcePoll;

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    awaited(&controller, row, |observed| {
        observed.download.admitted_bytes >= MAX_BYTES
    })
    .await;
    controller
        .pause_once(held)
        .expect("arm the transfer's source checkpoint");
    // The gate is opened before the wait: an owner parked on an empty channel is
    // woken by the frame it will be held in front of, so a row that waited first
    // would be waiting for a turn nothing can start.
    witnessed.release();
    wait_paused(&controller, held, row).await;
    awaited(&controller, row, |_| witnessed.sent() == 2).await;
    controller
        .release(held)
        .expect("release the transfer's source checkpoint");

    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        (delivered.status, delivered.bytes),
        (200, MAX_BYTES),
        "{row}: the peer keeps its committed status and no crossing byte"
    );
    assert_settled_polls(&controller, row, InboundTerminal::TransferBytes).await;
    drop(peer);
    stop(server, row);
    assert_released_once(&controller, row).await;
}

/// The deadline rows: the witnessed frame is queued inside the held selection.
///
/// Two holds, and both are needed. The first is in front of the owner's source
/// poll, and it is where the deadline is left to expire — so the turn that
/// resumes reads its source once, finds nothing, and carries a crossed deadline.
/// The second is in front of that turn's selection, and it is where the witnessed
/// frame is queued: after the source has been read and before the terminal is
/// fixed. An owner obeying its guard cannot have seen that frame; one that polls
/// again can.
async fn assert_deadline_terminal_polls_no_later_frame(
    row: &str,
    budget: TransferBudget,
    expected: InboundTerminal,
) {
    let witnessed = Witnessed::new();
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let server = port.serve(witnessed.router(0, 1, budget));
    let addr = server.addr();
    let source = LifecycleCheckpoint::BeforeTransferSourcePoll;
    let selection = LifecycleCheckpoint::BeforeTransferTerminalSelection;
    controller
        .pause_once(source)
        .expect("arm the transfer's source checkpoint");

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    wait_paused(&controller, source, row).await;
    controller
        .pause_once(selection)
        .expect("arm the transfer's selection checkpoint");
    // The deadline expires while the source poll is still in front of the owner,
    // so the turn that resumes is the one that carries it.
    tokio::time::sleep(CROSSED_TOTAL + CROSSED_IDLE).await;
    controller
        .release(source)
        .expect("release the transfer's source checkpoint");

    wait_paused(&controller, selection, row).await;
    witnessed.release();
    awaited(&controller, row, |_| witnessed.sent() == 1).await;
    controller
        .release(selection)
        .expect("release the deadline's selection turn");

    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        (delivered.status, delivered.bytes),
        (200, 0),
        "{row}: the committed status stands and no witnessed byte reaches the peer"
    );
    assert_settled_polls(&controller, row, expected).await;
    drop(peer);
    stop(server, row);
    assert_released_once(&controller, row).await;
}

/// Read the fixed terminal, then prove the poll count stops moving.
async fn assert_settled_polls(
    controller: &LifecycleController,
    row: &str,
    expected: InboundTerminal,
) {
    let observed = awaited(controller, row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(expected),
        "{row}: the staged bound is the terminal: {observed:?}"
    );
    assert_eq!(
        observed.download.terminals, 1,
        "{row}: the terminal is fixed once"
    );
    let polled = observed.download.frames_polled;
    // The witnessed frame is still in the channel, so a guard that let go would
    // read it while this row waits.
    tokio::time::sleep(CROSSED_IDLE).await;
    let settled = controller.transfers_observed();
    assert_eq!(
        settled.download.frames_polled, polled,
        "{row}: no frame is polled after the terminal is fixed: {settled:?}"
    );
}

/// Prove the owner released its source and producer exactly once.
async fn assert_released_once(controller: &LifecycleController, row: &str) {
    let observed = awaited(controller, row, |observed| observed.download.releases >= 1).await;
    assert_eq!(
        observed.download.releases, 1,
        "{row}: the owner released its source and producer once: {observed:?}"
    );
}

/// A peer that goes away releases the owner without a terminal of its own.
async fn assert_released_on_disconnect() {
    let row = "a departed peer";
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let server = port.serve(holding_stream(
        1,
        TransferBudget::unbounded()
            .with_idle(UNREACHED)
            .expect("the interval is accepted"),
    ));
    let addr = server.addr();

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let mut byte = [0_u8; 1];
    peer.read_exact(&mut byte)
        .await
        .expect("the committed head reached the peer");
    drop(peer);

    assert_released_once(&controller, row).await;
    stop(server, row);
}

/// A stopped server releases the owner of a stream still in flight.
///
/// The producer keeps publishing, so the owner keeps taking turns: the first turn
/// after the graceful transition mints the one aggregate deadline and arms the
/// wake that ends the transfer on it. A graceful stop does not cancel an admitted
/// operation before that deadline, which is why this row names a short one rather
/// than expecting the stop itself to take the stream away — and why the server is
/// held directly, so the row can read what the transfer did and then join the
/// server on its own terms.
async fn assert_released_on_shutdown() {
    let row = "a stopped server";
    let port = http_support::reserve_observed();
    let (listener, addr, controller) = port.into_owned_parts();
    let handle = camber::http::server(paced_stream(
        TransferBudget::unbounded()
            .with_idle(UNREACHED)
            .expect("the interval is accepted"),
    ))
    .policy(
        camber::http::ServerPolicy::default()
            .shutdown_timeout(SHUTDOWN_DEADLINE)
            .expect("the aggregate deadline is accepted"),
    )
    .serve_background(listener)
    .expect("the owned server requires a Tokio runtime");

    let mut peer = open_streaming(addr, "/stream", EVENT_TIMEOUT).await;
    let mut byte = [0_u8; 1];
    peer.read_exact(&mut byte)
        .await
        .expect("the committed head reached the peer");
    handle.shutdown();

    let observed = awaited(&controller, row, |observed| {
        observed.download.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.download.terminal,
        Some(InboundTerminal::ShutdownDeadline),
        "{row}: the aggregate deadline is the terminal: {observed:?}"
    );
    assert_released_once(&controller, row).await;

    // Teardown is this row's own, on every path: the graceful window has already
    // closed, so the server is cancelled and joined under a bound rather than
    // waited on for a completion its own deadline has passed.
    handle.cancel();
    assert!(
        tokio::time::timeout(SHUTDOWN_TIMEOUT, handle).await.is_ok(),
        "{row}: the cancelled server joined"
    );
}

/// 11.T3
///
/// The transfer sources extend the one declared precedence rather than keeping a
/// table of their own. Each row makes two sources ready inside one held
/// scheduling turn and reads which of them production named, and the retained
/// Step 6 pair is a required control: an extension that reordered the inbound
/// causes fails that row while every transfer row still passes.
#[test]
fn equal_ready_transfer_events_extend_inbound_precedence() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_bytes_outrank_deadlines().await;
                assert_idle_outranks_total().await;
                assert_upload_idle_outranks_transfer_idle().await;
                assert_retained_inbound_order().await;
            });
        })
        .expect("the transfer precedence runtime ran");
}

/// The byte maximum outranks both transfer deadlines ready beside it.
///
/// The hold is armed only once the maximum has already been reached, so the frame
/// waiting inside the held turn is the one that crosses it. Both deadlines expire
/// during that same hold.
async fn assert_bytes_outrank_deadlines() {
    assert_held_turn(
        "a crossing frame against both deadlines",
        TransferBudget::bounded(MAX_BYTES, CROSSED_IDLE, CROSSED_TOTAL)
            .expect("the equal-ready budget is accepted"),
        MAX_BYTES / FRAME.len() + 1,
        Some(MAX_BYTES),
        InboundTerminal::TransferBytes,
    )
    .await;
}

/// The quiet interval outranks the lifetime ready beside it.
///
/// Nothing is queued behind the hold, so no frame restarts the interval and both
/// deadlines are ready in the one turn.
async fn assert_idle_outranks_total() {
    assert_held_turn(
        "a quiet interval against a lifetime",
        TransferBudget::bounded(usize::MAX, CROSSED_IDLE, CROSSED_TOTAL)
            .expect("the equal-ready budget is accepted"),
        0,
        None,
        InboundTerminal::TransferIdle,
    )
    .await;
}

/// The request body's quiet interval outranks the transfer's, ready together.
///
/// Both are live on one upload turn, which is what makes this row the joint one:
/// the request deadline the admitted head minted and the transfer deadline the
/// route's upload policy named are weighed against each other by the one order,
/// and the request's row is the higher of the two.
async fn assert_upload_idle_outranks_transfer_idle() {
    let row = "a request interval against a transfer interval";
    let port = http_support::reserve_observed();
    let server = port.serve(
        multipart_router()
            .request_budget(
                RequestBudget::unbounded()
                    .with_body_idle(CROSSED_IDLE)
                    .expect("the request interval is accepted"),
            )
            .upload_budget(
                TransferBudget::unbounded()
                    .with_idle(CROSSED_IDLE)
                    .expect("the transfer interval is accepted"),
            ),
    );
    let addr = server.addr();

    let mut peer = stalled_upload(addr).await;
    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        delivered.status, 408,
        "{row}: the crossed interval is mapped once"
    );
    let observed = awaited(server.controller(), row, |observed| {
        observed.upload.terminal.is_some()
    })
    .await;
    assert_eq!(
        observed.upload.terminal,
        Some(InboundTerminal::BodyIdle),
        "{row}: the request's interval outranks the transfer's: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}

/// The retained Step 6 control: the inbound order this step extends is
/// unchanged.
///
/// A buffered route with no transfer owner at all, whose request interval and
/// request lifetime are both crossed. The interval is the higher row, exactly as
/// it was before the transfer terminals existed — an extension that inserted a
/// transfer row above it, or that reordered these two, fails here.
async fn assert_retained_inbound_order() {
    let row = "the retained inbound pair";
    let port = http_support::reserve_observed();
    let mut router = Router::new();
    router.post("/buffered", |_req: &Request| async {
        Response::text(200, "buffered")
    });
    let server = port.serve(
        router.request_budget(
            RequestBudget::bounded(CROSSED_IDLE, CROSSED_IDLE)
                .expect("the retained request budget is accepted"),
        ),
    );
    let addr = server.addr();

    let mut peer = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect the retained control");
    peer.write_all(
        b"POST /buffered HTTP/1.1\r\nHost: transfer.test\r\nContent-Length: 64\r\n\r\nshort",
    )
    .await
    .expect("write the withheld buffered payload");
    peer.flush().await.expect("flush the withheld payload");

    let delivered = read_streamed(&mut peer).await;
    assert_eq!(
        delivered.status, 408,
        "{row}: the request's own interval answered it"
    );
    let observed = server.controller().transfers_observed();
    assert_eq!(
        (observed.upload, observed.download),
        (Default::default(), Default::default()),
        "{row}: a buffered route resolves no transfer owner at all: {observed:?}"
    );
    drop(peer);
    stop(server, row);
}
