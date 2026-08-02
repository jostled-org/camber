//! The transferable guard's final owner is the response body, and a streaming
//! response completes at the body's end of stream.
//!
//! A streaming handler returns — and its service future drops — long before
//! the stream finishes. Nothing about that drop may resolve the signal, and
//! nothing about the producer closing its own channel may either: a closed
//! channel does not prove the frames still queued behind it reached Hyper.

use super::fixture::{BOUND, DRIVER_ONLY, QUIET, with_drain_window};
use super::peer::{open_produced_stream, read_to_close, read_until, send};
use super::probes::{Admission, CauseProbe, Report, relay};
use super::routes::{probe_router, route_sized_stream, route_staged_stream, route_stream};
use super::servers::with_owned_server;
use camber::RuntimeError;
use camber::http::{DisconnectCause, Request, Router, StreamSender};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

/// The producer's first chunk, small enough for the peer to read whole.
const CHUNK: &[u8] = b"first";

/// The delimiter that frames the first chunk's read.
const FRAMED_CHUNK: &[u8] = b"first\r\n";

/// The first chunk as a whole HTTP/1 chunked frame, size line included.
///
/// `read_until` guarantees only that its answer ends with the delimiter, so the
/// size line is the part of the framing an assertion has to state itself.
const FIRST_FRAME: &[u8] = b"5\r\nfirst\r\n";

/// One flood chunk. The peer reads none of them, so they queue inside Hyper.
const FLOOD_CHUNK: usize = 256 * 1024;

/// The flood chunk's bytes, built once. `StreamSender::send` takes anything
/// that converts into `Bytes`, and a static slice converts without copying.
const FLOOD_BODY: &[u8] = &[b'x'; FLOOD_CHUNK];

/// How many flood chunks the producer queues before closing its channel.
///
/// Enough buffers to back Hyper's write queue up against a peer that is not
/// reading, so the body is not polled again and its end of stream is provably
/// still ahead of the already-closed channel.
///
/// It must stay under the response channel's depth — 32 slots, private to
/// `http::stream` — because past that the producer blocks in `send` and never
/// drops its sender. The margin is what keeps this a claim about the coupling
/// rather than about the exact production number: matching that number leaves
/// no room, so any reduction of it would fail this case ten seconds later with
/// a message naming the test's own plumbing.
const FLOOD_CHUNKS: usize = 24;

/// Bound on the flooded read, with room for chunked framing.
const FLOOD_LIMIT: usize = FLOOD_CHUNK * FLOOD_CHUNKS * 2;

/// The body a declared-length stream promises and then delivers.
const DECLARED_BODY: &[u8] = b"a streaming response with a declared content length";

/// Send the first chunk, then the flood, one test-released stage at a time.
///
/// The sender is dropped explicitly and the close reported afterwards, so a
/// test watching for quiet after "the channel closed" starts that watch from a
/// channel that provably closed rather than from a hope the producer got there.
async fn produce_in_stages(
    mut stages: UnboundedReceiver<()>,
    sender: StreamSender,
    closed: Arc<Report<()>>,
) {
    stages
        .recv()
        .await
        .expect("the staged producer's first release never arrived");
    let _ = sender.send(CHUNK).await;
    stages
        .recv()
        .await
        .expect("the staged producer's flood release never arrived");
    flood(&sender).await;
    drop(sender);
    closed.send(());
}

/// Queue more than Hyper will hold before it stops polling for another frame.
async fn flood(sender: &StreamSender) {
    for _ in 0..FLOOD_CHUNKS {
        match sender.send(FLOOD_BODY).await {
            Ok(()) => {}
            Err(_) => return,
        }
    }
}

/// What the staged stream measured, in the order the test observed it.
struct StagedOutcome {
    quiet_after_handoff: bool,
    chunk: Box<[u8]>,
    quiet_after_chunk: bool,
    quiet_after_channel_closed: bool,
    delivered: usize,
    cause: DisconnectCause,
}

#[test]
fn streaming_service_future_drop_does_not_resolve_and_end_of_stream_is_completed() {
    let mut router = probe_router();
    let (closed, channel_closed) = relay::<()>();
    let (probe, admission, stages) =
        route_staged_stream(&mut router, "/staged", 1, move |stages, sender| {
            let closed = Arc::clone(&closed);
            produce_in_stages(stages, sender, closed)
        });

    let observed = with_owned_server(router, |addr| {
        // Handoff: the head is produced and the handler's future has returned
        // and dropped. The guard moved into the body, so nothing resolved.
        let mut client = open_produced_stream(addr, "/staged", &probe, "the staged stream's head");
        admission.assert_admitted();
        let quiet_after_handoff = probe.still_unresolved();

        // A produced chunk is not the completion point either.
        stages.advance();
        let chunk = read_until(&mut client, FRAMED_CHUNK);
        let quiet_after_chunk = probe.still_unresolved();

        // The producer floods and drops its sender. Waiting for its own report
        // is what makes the quiet below a watch over a closed channel rather
        // than a wait for the producer to get there.
        stages.advance();
        channel_closed
            .recv_timeout(BOUND)
            .expect("the producer never reported closing its channel");
        let quiet_after_channel_closed = probe.still_unresolved();

        let flooded = read_to_close(&mut client, FLOOD_LIMIT);
        let cause = probe.cause();
        drop(client);
        StagedOutcome {
            quiet_after_handoff,
            chunk,
            quiet_after_chunk,
            quiet_after_channel_closed,
            delivered: flooded.len(),
            cause,
        }
    });

    assert!(
        observed.quiet_after_handoff,
        "the service future dropping after handoff resolved the response signal"
    );
    assert_eq!(
        observed.chunk.as_ref(),
        FIRST_FRAME,
        "the first chunk did not reach the peer as one framed chunk: {:?}",
        String::from_utf8_lossy(&observed.chunk)
    );
    assert!(
        observed.quiet_after_chunk,
        "a produced chunk resolved the response signal before end of stream"
    );
    assert!(
        observed.quiet_after_channel_closed,
        "the producer closing its channel resolved the response signal while \
         its frames were still queued"
    );
    assert!(
        observed.delivered >= FLOOD_CHUNK * FLOOD_CHUNKS,
        "the peer received {} bytes of the flooded body",
        observed.delivered
    );
    assert_eq!(
        observed.cause,
        DisconnectCause::Completed,
        "a fully produced stream did not resolve Completed at end of stream"
    );
}

/// What the declared-length stream measured.
struct DeclaredOutcome {
    status: u16,
    body: Box<[u8]>,
    cause: DisconnectCause,
}

/// A streaming response that declares its `content-length` still reaches a
/// completion point.
///
/// This is the ordinary streaming-proxy case, and it does not go through the
/// end-of-stream rule the sibling case proves. Hyper's HTTP/1 encoder is chosen
/// from the headers, not from the body's size hint: with a declared length it
/// clears the body once that many bytes are written, without a final poll. A
/// body that only completes on `Ready(None)` therefore never completes at all,
/// and drops still armed — reporting a disconnect for a response delivered in
/// full.
#[test]
fn declared_content_length_stream_completes_when_fully_delivered() {
    let mut router = probe_router();
    let (declared, admission) = route_sized_stream(
        &mut router,
        "/declared",
        DECLARED_BODY.len(),
        |sender, _| async move {
            // A failed send would leave the peer short of the length the
            // response declared, which the body assertion below reports.
            let _ = sender.send(DECLARED_BODY).await;
        },
    );

    let observed = with_owned_server(router, |addr| {
        let response = send(addr, "GET", "/declared", "the declared-length stream");
        admission.assert_admitted();
        DeclaredOutcome {
            status: response.status,
            body: response.body,
            cause: declared.cause(),
        }
    });

    assert_eq!(
        observed.status, 200,
        "the declared-length 200 was not produced"
    );
    assert_eq!(
        observed.body.as_ref(),
        DECLARED_BODY,
        "the declared-length stream did not deliver the body it promised"
    );
    assert_eq!(
        observed.cause,
        DisconnectCause::Completed,
        "a streaming response delivered in full under its own declared \
         content-length did not resolve Completed"
    );
}

/// A streaming response that declares `content-length: 0` completes although
/// nothing is ever produced through it.
///
/// The sibling case above declares a length it then delivers, so its body is
/// polled. A zero length is the boundary neither it nor the chunked cases
/// reach: Hyper picks a zero-length encoder from the headers and never polls
/// the body at all, so a completion point that waits for a frame or for
/// `Ready(None)` is never reached, and the guard drops still armed — reporting
/// `PeerDisconnect` or `StreamReset` for a response that was delivered in full.
/// This is the empty upstream of a streaming proxy, not a corner case.
#[test]
fn zero_length_declared_stream_resolves_completed() {
    let mut router = probe_router();
    let (empty, admission) =
        route_sized_stream(&mut router, "/declared-empty", 0, |sender, _| async move {
            // Nothing to send: the declared length is zero, so the producer's
            // only job is to close the channel it can never write to.
            drop(sender);
        });

    let observed = with_owned_server(router, |addr| {
        let response = send(
            addr,
            "GET",
            "/declared-empty",
            "the zero-length declared stream",
        );
        admission.assert_admitted();
        DeclaredOutcome {
            status: response.status,
            body: response.body,
            cause: empty.cause(),
        }
    });

    assert_eq!(observed.status, 200, "the zero-length 200 was not produced");
    assert_eq!(
        observed.body.len(),
        0,
        "the zero-length declared stream carried a body"
    );
    assert_eq!(
        observed.cause,
        DisconnectCause::Completed,
        "a streaming response declaring content-length: 0 did not resolve \
         Completed, so a response delivered in full reported a disconnect"
    );
}

/// What the shutdown-window probe measured.
struct WindowOutcome {
    stream_status: u16,
    stream_body: usize,
    refusal: Option<RuntimeError>,
    cause: DisconnectCause,
    sse_status: u16,
    sse_body: usize,
}

fn assert_window_response(probed: &WindowOutcome) {
    assert!(
        matches!(probed.refusal, Some(RuntimeError::ScopeClosed)),
        "the closed scope did not refuse the producer: {:?}",
        probed.refusal
    );
    assert_eq!(probed.stream_status, 200, "the 200 was not produced");
    assert_eq!(
        probed.stream_body, 0,
        "a refused producer's response carried chunks"
    );
    assert_eq!(
        probed.cause,
        DisconnectCause::Completed,
        "a refused producer left its produced response unresolved"
    );
    assert_eq!(probed.sse_status, 200, "the SSE 200 was not produced");
    assert_eq!(
        probed.sse_body, 0,
        "a refused SSE producer's response carried events"
    );
}

#[test]
fn refused_producer_in_the_shutdown_window_completes_at_end_of_stream() {
    let mut router = probe_router();
    // The producer body is empty, so an admitted spawn resolves as fast as a
    // refused one and only the root scope's answer differs.
    let (window, admission) =
        route_stream(&mut router, "/window-stream", 1, |sender, _| async move {
            drop(sender);
        });
    // The SSE producer enters through the internal blocking helper, so its
    // refusal has no caller to answer: the response ends its own stream and
    // production records what was refused.
    router.get_sse("/window-sse", |_req: &Request, writer| writer.comment());
    // The zero-event response alone cannot tell a recorded refusal from a
    // dropped one: dropping the refused handle is what closes the channel
    // either way. What production recorded is the only witness.
    let recorded = crate::common::capture_events("/window-sse");

    let observed = with_drain_window(
        None,
        router,
        |_| (),
        DRIVER_ONLY,
        move |addr, _| {
            let stream = send(
                addr,
                "GET",
                "/window-stream",
                "the streaming response inside the window",
            );
            let refusal = admission.outcome();
            let cause = window.cause();
            let sse = send(
                addr,
                "GET",
                "/window-sse",
                "the SSE response inside the window",
            );
            WindowOutcome {
                stream_status: stream.status,
                stream_body: stream.body.len(),
                refusal,
                cause,
                sse_status: sse.status,
                sse_body: sse.body.len(),
            }
        },
    );

    let probed = observed
        .probed
        .expect("the drain never reported the child count the window is defined by");
    assert_window_response(&probed);
    assert!(
        recorded.recorded(&[
            "producer=sse",
            "path=/window-sse",
            "task scope is closed to admission",
            "root scope refused a per-response producer",
        ]),
        "the refused SSE producer was dropped without a record: {:?}",
        recorded.events()
    );
    assert!(
        observed.reached_zero,
        "the drain never observed its child count reach zero"
    );
}

/// Register a streaming route whose producer emits one chunk and returns.
///
/// A HEAD reaches it through the trie's fallback to the GET handler, so the
/// handler, the response, and the guard are the ones a GET would get. Only what
/// Hyper does with the body afterwards differs.
fn route_head_stream(router: &mut Router, path: &str) -> (CauseProbe, Admission) {
    route_stream(router, path, 1, |sender, _| async move {
        let _ = sender.send(CHUNK).await;
    })
}

/// What a HEAD against the streaming routes measured.
struct HeadOutcome {
    stream_status: u16,
    stream_body: usize,
    cause: DisconnectCause,
    sse_status: u16,
    sse_body: usize,
    /// The method of every producer entry the SSE route reported, in order.
    sse_entries: Box<[Box<str>]>,
}

/// Hyper refuses to write a body for a HEAD and drops that body unpolled, so a
/// streaming route has no frame to complete on and no end of stream to reach.
///
/// The response was produced in full all the same, and a response that finishes
/// producing never resolves as a disconnect cause. Nothing about a buffered
/// route proves this: `strip_body_if_head` empties its body, so it reports end
/// of stream and completes on shape alone.
///
/// Only the SSE half proves a producer was not started — the streaming route's
/// producer is spawned by the same handler a GET would run — and that half is a
/// negative claim, so a GET against the same route follows it. Each entry
/// carries the method that caused it: a shared, methodless queue would let a
/// HEAD's late entry be consumed by the control's read and report the defect
/// under test as a pass on both halves.
#[test]
fn head_against_a_streaming_route_completes_and_does_not_start_the_sse_producer() {
    let mut router = probe_router();
    let (head_stream, admission) = route_head_stream(&mut router, "/head-stream");
    // Reported from inside the SSE producer, which a HEAD must never start: its
    // events would be written to a channel Hyper will never read.
    let (entered, sse_entries) = relay::<Box<str>>();
    router.get_sse("/head-sse", move |req: &Request, writer| {
        entered.send(req.method().into());
        writer.comment()
    });

    let observed = with_owned_server(router, |addr| {
        let stream = send(
            addr,
            "HEAD",
            "/head-stream",
            "the HEAD against the streaming route",
        );
        admission.assert_admitted();
        let cause = head_stream.cause();
        let sse = send(addr, "HEAD", "/head-sse", "the HEAD against the SSE route");
        // Ordered: the quiet window covers the HEAD alone, the control's read
        // waits for the GET's own entry, and the last window is what a HEAD
        // entry arriving late would land in.
        let after_head = sse_entries.recv_timeout(QUIET).ok();
        send(addr, "GET", "/head-sse", "the GET against the SSE route");
        let after_get = sse_entries.recv_timeout(BOUND).ok();
        let trailing = sse_entries.recv_timeout(QUIET).ok();
        HeadOutcome {
            stream_status: stream.status,
            stream_body: stream.body.len(),
            cause,
            sse_status: sse.status,
            sse_body: sse.body.len(),
            sse_entries: [after_head, after_get, trailing]
                .into_iter()
                .flatten()
                .collect(),
        }
    });

    assert_eq!(
        observed.stream_status, 200,
        "the HEAD never reached the streaming route"
    );
    assert_eq!(
        observed.stream_body, 0,
        "the HEAD against the streaming route carried a body"
    );
    assert_eq!(
        observed.cause,
        DisconnectCause::Completed,
        "a HEAD against a streaming route resolved a disconnect cause for a \
         response that was produced in full"
    );
    assert_eq!(
        observed.sse_status, 200,
        "the HEAD never reached the SSE route"
    );
    assert_eq!(
        observed.sse_body, 0,
        "the HEAD against the SSE route carried events"
    );
    // One entry, and it is the control's: no producer ran for either HEAD, the
    // relay is wired, and nothing arrived late enough to have been a HEAD's.
    let entries: Box<[&str]> = observed.sse_entries.iter().map(Box::as_ref).collect();
    assert_eq!(
        entries.as_ref(),
        ["GET"],
        "the SSE route's producer entries were not the control's GET alone"
    );
}
