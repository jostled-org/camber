//! One HTTP/2 client, for the cases whose claim is about the h2 wire.
//!
//! Connect, handshake, spawn the driver, send the request, drain the body while
//! releasing flow-control capacity, drop the client, abort the driver, and read
//! the four outcomes that join can have. Two roots wrote that whole sequence out,
//! down to the panic strings, and every step of it is a step one copy could get
//! wrong on its own — a body drained without releasing capacity stalls the
//! sender, and a driver joined without reading its result discards a real fault
//! as a cancellation.
//!
//! The answer comes back as the same [`HttpResponse`] every other transport in
//! this suite hands over, so a root reading it needs no second answer type with
//! its own hand-copied accessors.

use bytes::Bytes;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::http::{HttpResponse, bounded, remaining};

/// Build one HTTP/2 request head.
///
/// The authority travels as `:authority`, which is where an HTTP/2 peer states
/// what an HTTP/1 peer states in `Host`. Written once so the per-request client
/// and the persistent one cannot disagree about what they sent.
fn h2_request_head(
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> ::http::Request<()> {
    headers
        .iter()
        .fold(
            ::http::Request::builder()
                .method(method)
                .uri(format!("http://{host}{path}")),
            |builder, (name, value)| builder.header(*name, *value),
        )
        .body(())
        .expect("the HTTP/2 request head is representable")
}

/// Read one stream's head and body into the answer every transport here hands
/// back.
///
/// `deadline` is the whole exchange's. The head and the body share it, so a
/// peer that answers slowly and then dribbles cannot spend a multiple of the
/// caller's budget.
async fn read_answer(response: h2::client::ResponseFuture, deadline: Instant) -> HttpResponse {
    try_read_answer(response, deadline)
        .await
        .expect("no HTTP/2 response head")
}

/// Read one stream's answer, or report why the stream ended without one.
///
/// The fallible half of [`read_answer`], for a case whose claim is that no
/// answer was possible: a declaration the framing layer refuses never becomes a
/// request head, so the peer ends the stream instead of answering it. Every
/// caller that expects an answer goes through [`read_answer`], which states that
/// expectation once.
async fn try_read_answer(
    response: h2::client::ResponseFuture,
    deadline: Instant,
) -> Result<HttpResponse, h2::Error> {
    let response = bounded(response, remaining(deadline), "HTTP/2 response head").await?;
    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                Box::from(name.as_str()),
                Box::from(String::from_utf8_lossy(value.as_bytes()).as_ref()),
            )
        })
        .collect();
    let body = drain_h2_body(
        response.into_body(),
        "HTTP/2 response body frame",
        remaining(deadline),
    )
    .await;
    Ok(HttpResponse::from_parts(status, headers, body))
}

/// Send one HTTP/2 request over a connection of its own and read the answer.
///
/// The authority travels as `:authority`, which is where an HTTP/2 peer states
/// what an HTTP/1 peer states in `Host`. The connection is opened and given up
/// per request: what these cases turn on is what one exchange was answered with,
/// and a shared connection would make one row's framing a property of the row
/// before it.
///
/// It is [`PersistentH2Client`] holding one exchange, because that is what it
/// is. Connecting, sending, and closing are each already written there, and a
/// second copy of the handshake and the driver join is a second thing that can
/// get either wrong.
///
/// `bound` covers the connection's setup and, separately, the exchange it
/// carries — the budget a persistent connection gives each of its streams.
/// Within each of those, every leg is handed what is left of one deadline
/// rather than starting a fresh copy of it, which is the rule
/// [`super::http::remaining`] states for the whole harness.
pub async fn h2_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    bound: Duration,
) -> HttpResponse {
    let mut client = PersistentH2Client::connect(addr, bound).await;
    let answered = client.send_complete(method, path, host, headers, b"").await;
    client.close().await;
    answered
}

/// One HTTP/2 connection held open across several streams.
///
/// The per-request client above opens and gives up a connection per exchange,
/// which makes one row's framing a property of the row before it. A case whose
/// claim IS the connection — that a refusal ended only its own stream and left
/// the rest usable — cannot be written through that, and cannot copy the
/// handshake and driver into itself either: a driver joined without reading its
/// result discards a real protocol fault as a cancellation.
///
/// This owns the sender and the driver, and nothing else. It frames requests
/// and respects the peer's flow-control window; it chooses no limit, no policy,
/// no mapping, and no status.
///
/// The driver is held in an `Option` so exactly one of the two teardown paths
/// takes it: [`Self::close`] on the path a case reaches after its assertions,
/// and `Drop` on the path an assertion that failed unwound through.
pub struct PersistentH2Client {
    sender: h2::client::SendRequest<Bytes>,
    driver: Option<tokio::task::JoinHandle<Result<(), h2::Error>>>,
    /// The budget one exchange on this connection runs under.
    bound: Duration,
}

impl PersistentH2Client {
    /// Connect, handshake, and take ownership of the connection driver.
    pub async fn connect(addr: SocketAddr, bound: Duration) -> Self {
        let deadline = Instant::now() + bound;
        let tcp = bounded(
            tokio::net::TcpStream::connect(addr),
            remaining(deadline),
            "HTTP/2 connect",
        )
        .await
        .expect("the HTTP/2 peer could not connect");
        let (sender, connection) = bounded(
            h2::client::handshake(tcp),
            remaining(deadline),
            "HTTP/2 handshake",
        )
        .await
        .expect("the HTTP/2 handshake did not complete");
        Self {
            sender,
            driver: Some(tokio::spawn(connection)),
            bound,
        }
    }

    /// Open one stream on this connection, ready-checked first.
    ///
    /// `end_of_stream` states whether the request is complete as sent, which is
    /// the difference between a body that is finished and one this caller still
    /// owes.
    async fn open(
        &mut self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
        end_of_stream: bool,
        deadline: Instant,
    ) -> (h2::client::ResponseFuture, h2::SendStream<Bytes>) {
        let mut ready = bounded(
            self.sender.clone().ready(),
            remaining(deadline),
            "HTTP/2 sender readiness",
        )
        .await
        .expect("the HTTP/2 sender never became ready");
        ready
            .send_request(h2_request_head(method, path, host, headers), end_of_stream)
            .expect("the HTTP/2 stream could not be opened")
    }

    /// Send one complete request whose body is finished as it is sent.
    pub async fn send_complete(
        &mut self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> HttpResponse {
        let deadline = Instant::now() + self.bound;
        let (response, mut stream) = self
            .open(method, path, host, headers, body.is_empty(), deadline)
            .await;
        match body.is_empty() {
            true => {}
            false => stream
                .send_data(Bytes::copy_from_slice(body), true)
                .expect("the HTTP/2 request body could not be sent"),
        }
        read_answer(response, deadline).await
    }

    /// Send one request head and withhold every byte of the body it declares.
    ///
    /// The stream is reset once the answer arrives, because a refusal decided
    /// from a declaration is the whole claim and the promised bytes are never
    /// coming.
    pub async fn send_withheld(
        &mut self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
    ) -> HttpResponse {
        self.try_send_withheld(method, path, host, headers)
            .await
            .expect("no HTTP/2 response head")
    }

    /// Send one request head the transport itself may refuse, and withhold the
    /// body it declares.
    ///
    /// `Err` is the peer ending this stream instead of answering it, which is
    /// what a declaration below the framing layer's own rules gets: no request
    /// head is ever constructed, so nothing above the transport could answer it.
    /// [`Self::send_withheld`] is this with the expectation of an answer stated,
    /// because a case whose claim is the answer should not have to unwrap one.
    pub async fn try_send_withheld(
        &mut self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
    ) -> Result<HttpResponse, h2::Error> {
        let deadline = Instant::now() + self.bound;
        let (response, mut stream) = self
            .open(method, path, host, headers, false, deadline)
            .await;
        let answered = try_read_answer(response, deadline).await;
        stream.send_reset(h2::Reason::NO_ERROR);
        answered
    }

    /// Open one download whose body the caller reads a frame at a time.
    ///
    /// For the post-commit rows: the head has to be observed before the terminal
    /// they trigger, and the body has to be readable up to the exact point the
    /// stream is reset. Nothing here drains to completion, because completion is
    /// what these rows prove does not happen.
    pub async fn open_download(&mut self, path: &str) -> H2Download {
        let deadline = Instant::now() + self.bound;
        let (response, mut stream) = self
            .open("GET", path, "localhost", &[], true, deadline)
            .await;
        // The request carries no body, so the send half is closed at once: a
        // stream left half-open would keep the peer waiting on payload the row
        // never means to send.
        let _ended = stream.send_data(Bytes::new(), true);
        H2Download {
            response: Some(response),
            body: None,
            status: 0,
            bytes: 0,
            stream,
            bound: self.bound,
        }
    }

    /// Open one stream whose body the caller sends frame by frame.
    ///
    /// [`Self::send_paced`] sends every frame it was given and then reads the
    /// answer, which no case whose claim is *when* a frame was taken can use.
    /// This hands the stream back instead, so a case can offer one frame, read
    /// what the peer did with it, and decide what to do next.
    pub async fn open_paced(
        &mut self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
    ) -> H2RequestStream {
        let deadline = Instant::now() + self.bound;
        let (response, stream) = self
            .open(method, path, host, headers, false, deadline)
            .await;
        H2RequestStream {
            response: Some(response),
            stream,
            bound: self.bound,
        }
    }

    /// Send one request whose body arrives as exactly the frames named.
    ///
    /// Each frame waits for the peer to grant window before it is sent, so the
    /// pacing is the peer's own and not a timer's. A peer that stops accepting
    /// partway through has answered already, which is what a mid-body refusal
    /// looks like from here.
    pub async fn send_paced(
        &mut self,
        method: &str,
        path: &str,
        host: &str,
        headers: &[(&str, &str)],
        frames: &[&[u8]],
    ) -> HttpResponse {
        let deadline = Instant::now() + self.bound;
        let (response, mut stream) = self
            .open(method, path, host, headers, false, deadline)
            .await;
        for frame in frames {
            match push_frame(&mut stream, frame, deadline).await {
                FramePush::Accepted => {}
                FramePush::PeerStopped => break,
                FramePush::Failed(error) => panic!("an HTTP/2 request body frame failed: {error}"),
            }
        }
        // Best effort: a peer that already refused has reset this stream, and
        // the answer below is what the case reads either way.
        let _ = stream.send_data(Bytes::new(), true);
        read_answer(response, deadline).await
    }

    /// Drop the sender, then join the driver exactly once.
    ///
    /// The driver is taken before the sender goes, so the `Drop` that runs on
    /// the way out has nothing left to abort and the join below is the only one.
    pub async fn close(mut self) {
        let driver = self.driver.take();
        // The sender is the last handle holding this connection open, so it goes
        // before the driver is read: the driver's own end is that close.
        drop(self);
        match driver {
            Some(driver) => join_driver(driver).await,
            None => {}
        }
    }
}

impl Drop for PersistentH2Client {
    /// Abort a driver [`PersistentH2Client::close`] never reached.
    ///
    /// Every case here closes only after a block of assertions, so a failed
    /// assertion unwinds past that close — and a driver left behind keeps its
    /// task and the TCP connection under it running for the rest of the binary.
    /// Aborting rather than joining, because a `Drop` cannot await, and the
    /// close path is where a driver's own result is read.
    fn drop(&mut self) {
        match self.driver.take() {
            Some(driver) => driver.abort(),
            None => {}
        }
    }
}

/// One request stream whose body its case sends a frame at a time.
///
/// Owned outright rather than borrowed from its connection, so a case can hold
/// one stream open and complete another on the same connection meanwhile —
/// which is the only way to tell backpressure that belongs to one stream from
/// backpressure that belongs to the connection under it.
pub struct H2RequestStream {
    /// Taken by [`Self::answer`], so the answer is read exactly once.
    response: Option<h2::client::ResponseFuture>,
    stream: h2::SendStream<Bytes>,
    /// The budget one leg of this exchange runs under.
    bound: Duration,
}

/// What one offered request-body frame established.
#[derive(Debug, Eq, PartialEq)]
pub enum H2Offer {
    /// The peer granted credit and took the frame.
    Sent,
    /// The bound expired with the peer still withholding credit.
    Withheld,
    /// The peer stopped accepting this stream's body, which is what a mid-body
    /// refusal looks like from the sending side.
    PeerStopped,
}

impl H2RequestStream {
    /// Offer one frame, giving the peer `bound` to grant the credit for it.
    ///
    /// The bound is per offer rather than per stream, because it is what the
    /// caller is measuring: a generous one asks whether the peer will ever take
    /// the frame, and a short one asks whether it is taking frames right now.
    pub async fn offer(&mut self, frame: &[u8], bound: Duration) -> H2Offer {
        match await_credit(&mut self.stream, frame.len(), bound).await {
            Credit::Granted => offered(send_frame(&mut self.stream, frame)),
            Credit::Withheld => H2Offer::Withheld,
            Credit::Closed => H2Offer::PeerStopped,
            Credit::Failed(error) => {
                panic!("the HTTP/2 connection failed while offering a frame: {error}")
            }
        }
    }

    /// End this request body.
    ///
    /// Best effort: a peer that already refused has reset this stream, and the
    /// answer is what the case reads either way.
    pub fn finish(&mut self) {
        let _ = self.stream.send_data(Bytes::new(), true);
    }

    /// Cancel this stream, leaving the body it declared unsent.
    pub fn reset(&mut self) {
        self.stream.send_reset(h2::Reason::CANCEL);
    }

    /// Read this stream's answer.
    pub async fn answer(&mut self) -> HttpResponse {
        let response = self
            .response
            .take()
            .expect("this HTTP/2 stream's answer was already read");
        read_answer(response, Instant::now() + self.bound).await
    }
}

/// Read one accepted push as the offer outcome it is.
///
/// The two vocabularies differ in one thing: an offer can end because credit
/// never came, and a push cannot. A connection fault is neither, on either side.
fn offered(push: FramePush) -> H2Offer {
    match push {
        FramePush::Accepted => H2Offer::Sent,
        FramePush::PeerStopped => H2Offer::PeerStopped,
        FramePush::Failed(error) => panic!("an HTTP/2 request body frame failed: {error}"),
    }
}

/// What one wait for request-stream flow-control credit established.
enum Credit {
    /// The peer granted enough credit to send the frame.
    Granted,
    /// The wait's bound expired with the credit still ungranted.
    Withheld,
    /// The peer ended this stream gracefully rather than granting more.
    Closed,
    /// The connection itself failed, and that is never a refusal.
    Failed(h2::Error),
}

/// Wait for the peer to grant enough credit to send `wanted` bytes.
///
/// Capacity is reserved and awaited rather than assumed: a sender that writes
/// past the window stalls, and the case waiting on the answer would report a
/// timeout for a server doing exactly what it was told. An expiring bound is
/// what separates this from [`push_frame`]: a case measuring backpressure needs
/// "no credit arrived" as an answer rather than as a panic.
async fn await_credit(
    stream: &mut h2::SendStream<Bytes>,
    wanted: usize,
    bound: Duration,
) -> Credit {
    let deadline = Instant::now() + bound;
    stream.reserve_capacity(wanted);
    loop {
        if stream.capacity() >= wanted {
            return Credit::Granted;
        }
        let granted = tokio::time::timeout(
            remaining(deadline),
            std::future::poll_fn(|cx| stream.poll_capacity(cx)),
        )
        .await;
        match granted {
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(error))) if is_graceful_end(&error) => return Credit::Closed,
            Ok(Some(Err(error))) => return Credit::Failed(error),
            Ok(None) => return Credit::Closed,
            Err(_) => return Credit::Withheld,
        }
    }
}

/// What one attempt to push a body frame established.
///
/// Three outcomes, because they mean three different things and only one of
/// them is the signal a paced upload is looking for. Collapsing them made a
/// server-side `INTERNAL_ERROR` read as "the peer refused mid-body": the loop
/// stopped, the answer never came, and the case reported the useless "no HTTP/2
/// response head" for a connection that had failed.
enum FramePush {
    /// The peer took the frame.
    Accepted,
    /// The peer stopped accepting this stream's body, which is what a mid-body
    /// refusal looks like from the sending side.
    PeerStopped,
    /// The connection itself failed, and that is never a refusal.
    Failed(h2::Error),
}

/// Push one frame through the peer's flow-control window.
///
/// The whole-body senders expect their frames to be taken, so a peer that never
/// grants the credit is a hang rather than an outcome and fails here. A case
/// whose claim IS the withheld credit reads it through
/// [`H2RequestStream::offer`] instead.
async fn push_frame(
    stream: &mut h2::SendStream<Bytes>,
    frame: &[u8],
    deadline: Instant,
) -> FramePush {
    let bound = remaining(deadline);
    match await_credit(stream, frame.len(), bound).await {
        Credit::Granted => send_frame(stream, frame),
        Credit::Closed => FramePush::PeerStopped,
        Credit::Failed(error) => FramePush::Failed(error),
        Credit::Withheld => {
            panic!("HTTP/2 request flow-control capacity timed out after {bound:?}")
        }
    }
}

/// Send one frame after the peer grants enough flow-control capacity.
fn send_frame(stream: &mut h2::SendStream<Bytes>, frame: &[u8]) -> FramePush {
    match stream.send_data(Bytes::copy_from_slice(frame), false) {
        Ok(()) => FramePush::Accepted,
        Err(error) => push_failure(error),
    }
}

/// Whether one `h2` failure is the peer ending this stream gracefully.
///
/// A stream reset carrying `NO_ERROR` is the stream-local disposition a refusal
/// applies to a request body it will never read. Any other reason is the
/// connection failing. Every side of every exchange here reads that distinction
/// through this one function, because the two sides of one exchange must not
/// disagree about which resets are an outcome.
fn is_graceful_end(error: &h2::Error) -> bool {
    error.reason() == Some(h2::Reason::NO_ERROR)
}

/// Read one `h2` send failure as the peer's disposition or as a fault.
fn push_failure(error: h2::Error) -> FramePush {
    match is_graceful_end(&error) {
        true => FramePush::PeerStopped,
        false => FramePush::Failed(error),
    }
}

/// Read one HTTP/2 response body to end of stream.
///
/// Capacity is released per frame because the window is the sender's budget: a
/// reader that took the bytes and never released it would stall the peer partway
/// through a body, and the case waiting on that body would report a timeout for
/// a server that was doing exactly what it was told.
///
/// `bound` covers the whole body, not one frame of it. A per-frame budget gives
/// a peer dribbling frames a fresh full bound for each one, so a body that never
/// ends outlasts its caller's deadline by as many frames as the peer cares to
/// send.
pub async fn drain_h2_body(
    mut body: h2::RecvStream,
    operation: &str,
    bound: Duration,
) -> Box<[u8]> {
    let deadline = Instant::now() + bound;
    let mut bytes = Vec::new();
    while let Some(chunk) = bounded(body.data(), remaining(deadline), operation).await {
        // A peer that resets this stream with NO_ERROR after answering has ended
        // it gracefully — the stream-local disposition a refusal applies to the
        // request body it will never read. Any other reason is a fault.
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) if is_graceful_end(&error) => break,
            Err(error) => panic!("an HTTP/2 body frame failed: {error}"),
        };
        body.flow_control()
            .release_capacity(chunk.len())
            .expect("the HTTP/2 reader could not release its flow-control capacity");
        bytes.extend_from_slice(&chunk);
    }
    bytes.into_boxed_slice()
}

/// End the connection driver and read the four outcomes that can have.
///
/// The response is already complete, so the driver is aborted rather than waited
/// on — the server's own keepalive policy decides when it would otherwise close.
/// Only the driver's own cancellation is an accepted end: a protocol failure or
/// a panic inside it is a fault, and a join that only checked for cancellation
/// would discard both.
async fn join_driver(driver: tokio::task::JoinHandle<Result<(), h2::Error>>) {
    driver.abort();
    match driver.await {
        Ok(Ok(())) => {}
        Err(error) if error.is_cancelled() => {}
        Ok(Err(error)) => panic!("HTTP/2 client driver failed: {error}"),
        Err(error) => panic!("HTTP/2 client driver join failed: {error}"),
    }
}

/// What one HTTP/2 peer saw of a committed streaming response.
#[derive(Debug, Eq, PartialEq)]
pub struct H2Streamed {
    pub status: u16,
    /// Payload bytes the peer actually received.
    pub bytes: usize,
    /// Whether the stream was reset under its committed head, rather than ended.
    pub reset: bool,
}

/// One download whose committed head and partial body its case reads itself.
pub struct H2Download {
    response: Option<h2::client::ResponseFuture>,
    body: Option<h2::RecvStream>,
    status: u16,
    bytes: usize,
    stream: h2::SendStream<Bytes>,
    bound: Duration,
}

impl H2Download {
    /// Read this download's committed head, leaving its body on the stream.
    pub async fn head(&mut self) -> u16 {
        let response = self
            .response
            .take()
            .expect("this HTTP/2 download's head was already read");
        let response = bounded(response, self.bound, "HTTP/2 download head")
            .await
            .expect("no HTTP/2 download head");
        self.status = response.status().as_u16();
        self.body = Some(response.into_body());
        self.status
    }

    /// Read this download until its stream ends or is reset.
    ///
    /// The head is read first when a case has not already read it, so one call
    /// covers the rows whose cause is the producer and the rows whose cause is
    /// something the case does between the head and the body.
    pub async fn drain(&mut self) -> H2Streamed {
        if self.response.is_some() {
            self.head().await;
        }
        let deadline = Instant::now() + self.bound;
        let mut body = self
            .body
            .take()
            .expect("this HTTP/2 download's body was already drained");
        let mut reset = false;
        while let Some(chunk) =
            bounded(body.data(), remaining(deadline), "HTTP/2 download body").await
        {
            match chunk {
                Ok(chunk) => {
                    body.flow_control()
                        .release_capacity(chunk.len())
                        .expect("the HTTP/2 reader could not release its capacity");
                    self.bytes += chunk.len();
                }
                // A reset under an answered head is the stream-local disposition
                // a post-commit terminal applies. It is this row's answer, not a
                // fault: the status is already on the wire and cannot change.
                Err(_) => {
                    reset = true;
                    break;
                }
            }
        }
        H2Streamed {
            status: self.status,
            bytes: self.bytes,
            reset,
        }
    }

    /// Cancel this stream, leaving the body it was given unread.
    pub fn reset(&mut self) {
        self.stream.send_reset(h2::Reason::CANCEL);
    }
}
