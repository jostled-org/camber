//! One owner-backed immutable payload, and the multi-client direct WebSocket
//! row that admits it.
//!
//! A shared-payload claim needs two things no ordinary fixture supplies: a
//! `Bytes` value whose backing allocation says when it is released, and more
//! than one live connection over which one such value is cloned. Both are built
//! here on the public route, the public halves, and the listener-scoped
//! checkpoints every other direct-WebSocket row already uses, so nothing here
//! reaches past what an application can reach.
//!
//! Every peer, half, worker, checkpoint, and server this module opens belongs to
//! [`DirectionTestFixture`], whose bounded teardown protocol runs on the failing
//! path as well as the passing one.

#![cfg(feature = "ws")]

use std::future::Future;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// The allocation oracle installs its own global allocator, and two global
// allocators do not link. Every item that reaches the probe is built only where
// it is, exactly as every other measured row in the suite is.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
use allocation_counter::AllocationInfo;
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
use camber::RuntimeError;
use camber::http::{Bytes, WsReceiver, WsSender};

use super::http::is_closed_connection_error;
use super::ws::try_read_ws_frame_raw;
use super::ws_async::lifecycle_event;
use super::ws_directions::{
    CLOSE, DIRECTION_PATH, DirectionHandoff, DirectionTestFixture, direction_router,
};

/// The small payload every calibrated row measures against.
pub const SMALL_PAYLOAD: usize = 64;

/// The large payload every calibrated row measures against.
///
/// Sixteen thousand times the small one, so a single per-recipient copy of it
/// cannot hide inside the fixed bookkeeping allowance below.
pub const LARGE_PAYLOAD: usize = 1_048_576;

/// How many bytes of fixed per-admission bookkeeping a measured window may
/// differ by between the two payload sizes.
///
/// One page. Admitting a shared payload moves a handle, so the only thing that
/// may grow with the payload's own length is a copy — and a copy of the large
/// payload is two hundred and fifty-six times this.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
const FLAT_BYTES: u64 = 4096;

/// How many allocations a measured window may differ by between the two payload
/// sizes.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
const FLAT_COUNT: u64 = 2;

/// The fanouts every calibrated and delivery row runs at.
pub const FANOUTS: [usize; 4] = [1, 2, 16, 100];

/// How one immutable payload's backing allocation reports its own release.
///
/// A flag and a report, because a row asks two different questions of it: it
/// asserts the backing is still alive at an exact production checkpoint, which
/// has to answer now, and it waits for the release after a bridge completes,
/// which has to answer without a poll loop or an elapsed-time guess.
struct PayloadRelease {
    dropped: AtomicBool,
    reported: tokio::sync::mpsc::UnboundedSender<()>,
}

impl PayloadRelease {
    /// Record the release, and report it to whoever is waiting.
    fn fire(&self) {
        self.dropped.store(true, Ordering::Release);
        let _ = self.reported.send(());
    }
}

/// One immutable payload's backing allocation.
///
/// The whole point is the `Drop`: a `Bytes` built over this owner keeps it alive
/// for exactly as long as some handle on that payload exists, so a clone the
/// production path retained and a clone it copied away from are told apart by
/// whether this has fired.
struct PayloadOwner {
    bytes: Box<[u8]>,
    release: Arc<PayloadRelease>,
}

impl AsRef<[u8]> for PayloadOwner {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for PayloadOwner {
    fn drop(&mut self) {
        self.release.fire();
    }
}

/// What one payload's backing allocation has done, seen from the case.
pub struct PayloadWitness {
    release: Arc<PayloadRelease>,
    released: tokio::sync::mpsc::UnboundedReceiver<()>,
    subject: Box<str>,
}

impl PayloadWitness {
    /// Whether some handle still keeps the backing allocation alive.
    fn live(&self) -> bool {
        !self.release.dropped.load(Ordering::Acquire)
    }

    /// Require that some handle still keeps the backing allocation alive.
    pub fn assert_live(&self, what: &str) {
        assert!(
            self.live(),
            "{what}: {} was already released, so something copied the payload away from it",
            self.subject
        );
    }

    /// Wait, bounded, for the last handle on the backing allocation to go.
    ///
    /// The report arrives on a channel rather than through a poll loop: the
    /// release is an event the owner publishes from its own `Drop`, and a row
    /// that spun on a flag would be claiming the release from elapsed time.
    ///
    /// The witness holds the reporting end's own `Arc`, so the channel cannot
    /// answer by closing. Either the owner fires, or the row names the handle
    /// still holding it at the deadline.
    pub async fn assert_released(&mut self, what: &str) {
        lifecycle_event(
            &format!("{what}: {} to be released", self.subject),
            self.released.recv(),
        )
        .await;
    }
}

/// Exactly `len` bytes whose values depend on `tag`.
///
/// A repeating four-byte pattern rather than a counter, so no cast is needed to
/// make a byte out of an index and two rows still cannot be confused at a peer.
pub fn payload_bytes(len: usize, tag: u8) -> Box<[u8]> {
    let pattern = [tag, tag ^ 0x5a, tag ^ 0xa5, tag ^ 0xff];
    (0..len)
        .map(|index| pattern[index % pattern.len()])
        .collect()
}

/// One immutable payload over exactly `bytes`, and the witness for its backing
/// allocation.
pub fn witnessed_payload(bytes: &[u8], subject: &str) -> (Bytes, PayloadWitness) {
    let (reported, released) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(PayloadRelease {
        dropped: AtomicBool::new(false),
        reported,
    });
    let payload = Bytes::from_owner(PayloadOwner {
        bytes: bytes.into(),
        release: Arc::clone(&release),
    });
    (
        payload,
        PayloadWitness {
            release,
            released,
            subject: subject.into(),
        },
    )
}

/// Admit `clones` clones of one owner-backed payload through `sender`, and give
/// up the caller's own handle.
///
/// The caller keeps only the witness, so whatever holds the backing alive from
/// here is the connection and never the row. Stated once because every
/// retention and release row wants exactly this, and a row that spelled it out
/// again is a row that can forget to drop its original — which keeps the
/// witness live and passes every release claim without the production path
/// having released anything.
pub fn offer_shared_clones(
    sender: &WsSender,
    clones: usize,
    bytes: &[u8],
    subject: &str,
) -> PayloadWitness {
    let (payload, witness) = witnessed_payload(bytes, subject);
    for _ in 0..clones {
        sender
            .try_send_shared_binary(payload.clone())
            .expect("admit one shared clone");
    }
    drop(payload);
    witness
}

/// One direct connection a shared-payload row owns end to end.
///
/// The halves are optional because a row gives them up at a moment of its own
/// choosing — dropping the unique receive owner is itself a terminal event —
/// while the peer stays, so the same row can still read what the ending bridge
/// owed it.
pub struct SharedPayloadClient {
    peer: TcpStream,
    sender: Option<WsSender>,
    receiver: Option<WsReceiver>,
}

impl SharedPayloadClient {
    /// This connection's send capability, while the row still holds it.
    pub fn sender(&self) -> &WsSender {
        self.sender
            .as_ref()
            .expect("the row already gave up this connection's send capability")
    }

    /// This connection's raw client socket.
    pub fn peer(&mut self) -> &mut TcpStream {
        &mut self.peer
    }

    /// Take this connection's unique receive owner, so a row can drop it.
    pub fn take_receiver(&mut self) -> WsReceiver {
        self.receiver
            .take()
            .expect("the row already took this connection's receive owner")
    }

    /// Give up both public halves, keeping the peer.
    pub fn release_halves(&mut self) {
        self.sender = None;
        self.receiver = None;
    }
}

/// Everything one shared-payload row serves, connects, splits, and holds.
pub struct SharedPayloadFixture {
    listener: Arc<DirectionTestFixture>,
    clients: Box<[SharedPayloadClient]>,
}

impl SharedPayloadFixture {
    /// The listener, server, checkpoints, and workers this row runs against.
    pub fn listener(&self) -> &DirectionTestFixture {
        &self.listener
    }

    /// How many real connections this row fans out to.
    pub fn recipients(&self) -> usize {
        self.clients.len()
    }

    /// One recipient, for the length of one read or one half's release.
    pub fn client(&mut self, index: usize) -> &mut SharedPayloadClient {
        &mut self.clients[index]
    }

    /// One recipient's send capability, without borrowing the row mutably.
    pub fn sender(&self, index: usize) -> &WsSender {
        self.clients[index].sender()
    }

    /// Every recipient's send capability, in connection order.
    ///
    /// Handles rather than borrows, so a measured window can name a subset of
    /// them while the row goes on reading its peers.
    pub fn senders(&self) -> Box<[WsSender]> {
        self.clients
            .iter()
            .map(|client| client.sender().clone())
            .collect()
    }

    /// Admit `clones` clones of one owner-backed payload to recipient `index`,
    /// and give up this row's own handle.
    ///
    /// [`offer_shared_clones`], reached through the recipient the row names.
    pub fn offer_clones(
        &self,
        index: usize,
        clones: usize,
        bytes: &[u8],
        subject: &str,
    ) -> PayloadWitness {
        offer_shared_clones(self.sender(index), clones, bytes, subject)
    }

    /// Admit and drain one small shared frame on every recipient from `first`.
    ///
    /// A measured window taken cold counts the queue block and waker each
    /// connection allocates on its first admission. That cost is fixed per
    /// connection rather than per payload byte, so leaving it in would land it
    /// on whichever payload size ran first and turn the comparison below into a
    /// claim about ordering.
    pub fn warm_queues_from(&mut self, first: usize) {
        let warmed = payload_bytes(SMALL_PAYLOAD, WARM_TAG);
        let payload = Bytes::copy_from_slice(&warmed);
        for client in &self.clients[first..] {
            client
                .sender()
                .try_send_shared_binary(payload.clone())
                .expect("admit the warming frame");
        }
        self.take_peer_frames_from(first, &warmed, "the warming frame");
    }

    /// Read one binary frame from every recipient from `first`, and require its
    /// bytes are exactly `expected`.
    ///
    /// The range has to hold a recipient. A row that named one past its own
    /// fanout, or that read after giving up its connections, would otherwise
    /// take no frame at all and report that as delivery.
    pub fn take_peer_frames_from(&mut self, first: usize, expected: &[u8], what: &str) {
        assert!(
            first < self.clients.len(),
            "{what}: the row holds no recipient at or past {first}"
        );
        for (offset, client) in self.clients[first..].iter_mut().enumerate() {
            let recipient = first + offset;
            let (opcode, payload) =
                try_read_ws_frame_raw(&mut client.peer).unwrap_or_else(|error| {
                    panic!("{what}: recipient {recipient}'s read answered {error}")
                });
            assert_eq!(
                opcode, BINARY,
                "{what}: recipient {recipient} took opcode {opcode:#x}"
            );
            assert_payload_bytes(&payload, expected, what);
        }
    }

    /// Give up every public half this row owns, keeping every peer.
    pub fn release_every_half(&mut self) {
        for client in &mut self.clients {
            client.release_halves();
        }
    }

    /// Let go of every connection this row owns, peers and halves alike.
    ///
    /// What a row does before it joins its server. A peer left open is a peer
    /// the closing bridge still owes a close handshake, and one that never
    /// answers holds the connection until the server's own stop deadline
    /// expires — which is a hung row reported as a timeout rather than as the
    /// claim it was making.
    pub fn end_every_connection(&mut self) {
        self.clients = Box::default();
    }

    /// Ask this row's server to stop, and wait for it to finish.
    ///
    /// The tail every release row shares. A row that spelled it out again could
    /// wait on the join without having asked for the stop, which reports the
    /// row's own omission as the bridge failing to complete.
    pub async fn stop_and_join(&self) {
        self.listener.shutdown_server();
        self.listener
            .join_server()
            .await
            .expect("the owned server completed");
    }
}

/// The WebSocket binary opcode, as it appears on the wire.
const BINARY: u8 = 0x02;

/// The payload a warming admission carries.
const WARM_TAG: u8 = 0x11;

/// The aggregate shutdown grace a `#[camber::test]` runtime establishes.
///
/// Named because every row below runs under a runtime of its own and has to
/// stop its server under the same bound the case runtime would have given it.
const ROW_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// The pre-head wait a `#[camber::test]` runtime establishes.
const ROW_HEADER_TIMEOUT: Duration = Duration::from_millis(100);

/// Serve one capacity-`buffer` direct route, connect `recipients` real peers,
/// and run `case` against every connection the production callback was given.
///
/// One runner for every shared-payload row, because each of them needs the same
/// listener, the same route, the same split, and the same bounded teardown; a
/// row that opened its own would be the second place a leaked peer, half, or
/// checkpoint could hide.
///
/// Each row runs on a thread and a runtime of its own, and that is not an
/// optimisation. The aggregate shutdown deadline is minted by the first graceful
/// transition anywhere in a runtime and is never restarted, so rows sharing one
/// runtime share one grace: the second stops under whatever the first left of
/// it, and a later one under none of it at all — a `Timeout` that reports the
/// row before it rather than the claim being made. A case that runs several of
/// these in sequence is the ordinary shape here, so the isolation belongs to the
/// runner rather than to each case that remembers to ask for it.
pub fn shared_payload_row<C, Fut>(buffer: usize, recipients: usize, case: C)
where
    C: FnOnce(SharedPayloadFixture) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let row = std::thread::spawn(move || {
        camber::runtime::builder()
            .header_timeout(ROW_HEADER_TIMEOUT)
            .shutdown_timeout(ROW_SHUTDOWN_TIMEOUT)
            .run(|| camber::runtime::block_on(serve_shared_payload_row(buffer, recipients, case)))
            .expect("the shared-payload row runtime failed");
    });
    // Resumed rather than reported: the row's own assertion is the failure, and
    // a join that only said "the row panicked" would replace it with a sentence
    // naming neither the claim nor the value it got.
    match row.join() {
        Ok(()) => {}
        Err(unwound) => std::panic::resume_unwind(unwound),
    }
}

/// One shared-payload row, inside the runtime that owns its stop.
async fn serve_shared_payload_row<C, Fut>(buffer: usize, recipients: usize, case: C)
where
    C: FnOnce(SharedPayloadFixture) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, mut handoff) = direction_router(DIRECTION_PATH, buffer);
    DirectionTestFixture::run(
        |listener| {
            camber::http::serve_background(listener, router)
                .expect("owned server requires a Tokio runtime")
        },
        |listener| async move {
            listener.hold_callbacks(handoff.take_gate());
            let clients = connect_clients(&listener, &mut handoff, recipients).await;
            case(SharedPayloadFixture { listener, clients }).await;
            drop(handoff);
        },
    )
    .await;
}

/// Open every peer this row fans out to, and split the connection each one's
/// production callback was given.
async fn connect_clients(
    listener: &DirectionTestFixture,
    handoff: &mut DirectionHandoff,
    recipients: usize,
) -> Box<[SharedPayloadClient]> {
    let mut clients = Vec::with_capacity(recipients);
    for _ in 0..recipients {
        let peer = listener.connect(DIRECTION_PATH);
        let (sender, receiver) = handoff.connection().await.split();
        clients.push(SharedPayloadClient {
            peer,
            sender: Some(sender),
            receiver: Some(receiver),
        });
    }
    clients.into_boxed_slice()
}

/// Clone one shared payload into every sender given, measuring only this
/// thread's own allocation.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
pub fn measure_shared_admission(senders: &[WsSender], payload: &Bytes) -> AllocationInfo {
    measure_admission(senders, "shared", |sender| {
        sender.try_send_shared_binary(payload.clone())
    })
}

/// The same window through the shipped borrowed-slice admission.
///
/// The copied control, and a shipped operation rather than a synthetic one: a
/// control a test wrote could drift from what an application actually pays.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
pub fn measure_borrowed_admission(senders: &[WsSender], payload: &[u8]) -> AllocationInfo {
    measure_admission(senders, "borrowed", |sender| {
        sender.try_send_binary(payload)
    })
}

/// One admission window, taken over every sender given.
///
/// A refusal is carried out of the window rather than asserted inside it:
/// formatting an assertion allocates, and an assertion here would measure the
/// harness instead of the admission. Building the error itself allocates
/// nothing, so the row still reports which refusal it met. The measurement is
/// thread-local, so the transport workers writing these frames cannot reach it.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
fn measure_admission(
    senders: &[WsSender],
    kind: &str,
    mut admit: impl FnMut(&WsSender) -> Result<(), RuntimeError>,
) -> AllocationInfo {
    assert_probe_calibrated();
    let mut refusal = None;
    let measured = allocation_counter::measure(|| {
        for sender in senders {
            match admit(sender) {
                Ok(()) => {}
                Err(error) => refusal = refusal.take().or(Some(error)),
            }
        }
    });
    match refusal {
        Some(error) => {
            panic!("a {kind} admission was refused inside the measured window: {error}")
        }
        None => {}
    }
    measured
}

/// Prove the allocation probe can see one allocation before a row trusts it to
/// report none.
///
/// A probe that observed nothing would make every flatness bound below
/// unfalsifiable, and each test binary has to establish that for itself.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
fn assert_probe_calibrated() {
    let calibration = allocation_counter::measure(|| {
        drop(std::hint::black_box(Box::new(1_u32)));
    });
    assert!(
        calibration.count_total > 0,
        "the allocation probe counted nothing for one deliberate allocation"
    );
}

/// Require that admitting the large payload cost the caller no more than
/// admitting the small one.
///
/// Both byte figures and the count are bounded, because a per-recipient copy
/// could otherwise hide in a peak the totals average away.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
pub fn assert_payload_flat(small: &AllocationInfo, large: &AllocationInfo, what: &str) {
    assert!(
        large.bytes_total <= small.bytes_total + FLAT_BYTES,
        "{what}: admitting {LARGE_PAYLOAD} bytes allocated {} against {} for {SMALL_PAYLOAD}",
        large.bytes_total,
        small.bytes_total
    );
    assert!(
        large.bytes_max <= small.bytes_max + FLAT_BYTES,
        "{what}: admitting {LARGE_PAYLOAD} bytes peaked at {} against {} for {SMALL_PAYLOAD}",
        large.bytes_max,
        small.bytes_max
    );
    assert!(
        large.count_total <= small.count_total + FLAT_COUNT,
        "{what}: admitting {LARGE_PAYLOAD} bytes took {} allocations against {} for {SMALL_PAYLOAD}",
        large.count_total,
        small.count_total
    );
}

/// Require that the borrowed control paid one payload-sized copy per recipient.
///
/// Without this the bound above is unfalsifiable: a measurement that counted
/// nothing at all would satisfy it, and only a control that does copy shows the
/// oracle can tell the two apart.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
pub fn assert_borrowed_copies_per_recipient(
    small: &AllocationInfo,
    large: &AllocationInfo,
    recipients: usize,
    what: &str,
) {
    let owed = copied_bytes(recipients);
    assert!(
        large.bytes_total >= small.bytes_total + owed,
        "{what}: the borrowed control paid {} against {} and owed at least {owed} more",
        large.bytes_total,
        small.bytes_total
    );
    assert_recipient_allocations(small.count_total, recipients, SMALL_PAYLOAD, what);
    assert_recipient_allocations(large.count_total, recipients, LARGE_PAYLOAD, what);
}

/// What one recipient's copy of the large payload costs over the small one,
/// across the whole fanout.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
fn copied_bytes(recipients: usize) -> u64 {
    let per_recipient =
        u64::try_from(LARGE_PAYLOAD - SMALL_PAYLOAD).expect("the payload sizes fit a 64-bit count");
    let recipients = u64::try_from(recipients).expect("the fanout fits a 64-bit count");
    per_recipient * recipients
}

/// Require the borrowed control took at least one allocation per recipient.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
fn assert_recipient_allocations(count: u64, recipients: usize, len: usize, what: &str) {
    let owed = u64::try_from(recipients).expect("the fanout fits a 64-bit count");
    assert!(
        count >= owed,
        "{what}: the borrowed control took {count} allocations for {recipients} copies of {len} bytes"
    );
}

/// Require one frame's payload is exactly `expected`.
///
/// The whole slices are compared first, which is one `memcmp` — a row that
/// delivers a megabyte to each of a hundred peers walks the bytes by hand only
/// where they already differ. The hand walk is what names the first differing
/// byte, because `assert_eq!` would print a megabyte of both sides on a one-byte
/// mismatch and bury the claim.
pub fn assert_payload_bytes(taken: &[u8], expected: &[u8], what: &str) {
    match taken == expected {
        true => return,
        false => {}
    }
    assert_eq!(
        taken.len(),
        expected.len(),
        "{what}: the payload was {} bytes rather than {}",
        taken.len(),
        expected.len()
    );
    match taken
        .iter()
        .zip(expected)
        .position(|(took, owed)| took != owed)
    {
        Some(index) => panic!(
            "{what}: byte {index} was {:#x} rather than {:#x}",
            taken[index], expected[index]
        ),
        None => {}
    }
}

/// Read one peer's transport to its end, requiring nothing but closes.
///
/// The end itself has to arrive: a read that expired is a bridge that neither
/// wrote nor let go, which is exactly the retention these rows exist to catch.
pub fn assert_no_further_payload(peer: &mut TcpStream, what: &str) {
    let mut closes = 0_usize;
    let ended = loop {
        match try_read_ws_frame_raw(peer) {
            // One close is what an ending bridge owes this peer. A second is a
            // transport that keeps answering, and reading on would trade this
            // row's deadline for the binary's.
            Ok((CLOSE, _)) => closes += 1,
            Ok((opcode, payload)) => {
                panic!(
                    "{what}: the peer took opcode {opcode:#x} with {} bytes",
                    payload.len()
                )
            }
            Err(error) => break error,
        }
        assert!(closes < 2, "{what}: the peer took {closes} close frames");
    };
    match ended.kind() {
        std::io::ErrorKind::UnexpectedEof => {}
        _ if is_closed_connection_error(&ended) => {}
        kind => {
            panic!("{what}: the peer's transport answered {kind:?} rather than ending: {ended}")
        }
    }
}
