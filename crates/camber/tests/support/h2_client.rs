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
use std::task::Poll;
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
    let headers = header_pairs(response.headers());
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
        send_whole_body(&mut stream, body).await;
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
        // The request carries no body, so the send half is closed in the head
        // itself: a stream left half-open would keep the peer waiting on payload
        // the row never means to send.
        let (response, stream) = self
            .open("GET", path, "localhost", &[], true, deadline)
            .await;
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

    /// Let this connection end itself, then read its driver's result.
    ///
    /// [`Self::close`] aborts the driver, because most cases close with a stream
    /// the peer is still holding and will never answer — a connection nothing
    /// can end, which a join would wait on forever. The abort is what makes that
    /// close return, and it is also what makes it a kill: a `RST_STREAM` the
    /// case queued may never reach the wire, and the `GOAWAY` behind it never
    /// does.
    ///
    /// A case whose claim IS that its peer saw the cancellation cannot use that.
    /// This is for the cases that have released every stream they opened: the
    /// connection has nothing left to wait for, so dropping the sender ends it,
    /// and the frames the case queued go out before it does.
    ///
    /// Bounded, because a case that still owes a stream would otherwise hang the
    /// whole binary here rather than say which claim it got wrong.
    pub async fn close_settled(mut self) {
        let driver = self.driver.take();
        let bound = self.bound;
        drop(self);
        let Some(driver) = driver else {
            return;
        };
        let abort = driver.abort_handle();
        match tokio::time::timeout(bound, driver).await {
            Ok(joined) => report_driver(joined),
            Err(_) => {
                abort.abort();
                panic!("the HTTP/2 connection did not end after its last stream was released")
            }
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
            Credit::Granted => offered(send_frame(&mut self.stream, frame, false).await),
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

    /// Read this stream's committed head, leaving its body unread.
    ///
    /// [`Self::answer`] reads the head and drains the body in one call, which
    /// no case whose request half outlives the head can use: a full-duplex
    /// exchange still owes request frames while the answer is already on the
    /// wire. This hands the read half back instead, so a case can commit the
    /// head, send more payload, and settle the download afterwards.
    pub async fn commit(&mut self) -> H2ReadHalf {
        let response = self
            .response
            .take()
            .expect("this HTTP/2 stream's answer was already read");
        let response = bounded(response, self.bound, "HTTP/2 response head")
            .await
            .expect("no HTTP/2 response head");
        let status = response.status().as_u16();
        let headers = header_pairs(response.headers());
        H2ReadHalf {
            status,
            headers,
            body: response.into_body(),
            bound: self.bound,
        }
    }
}

/// The read half of one committed exchange, and what it ended on.
///
/// Held apart from the sending half because a full-duplex exchange's two
/// directions end independently: the answer can be settled while the request
/// body is still open, and the request body can fail after the answer's head is
/// already on the wire.
pub struct H2ReadHalf {
    status: u16,
    headers: Box<[(Box<str>, Box<str>)]>,
    body: h2::RecvStream,
    bound: Duration,
}

/// What one settled download carried, down to the trailers behind it.
///
/// The trailers are read rather than folded into the headers: a gRPC status
/// lives in exactly one of the two, and a case proving which owner produced it
/// cannot be written against a map that merged them.
#[derive(Debug)]
pub struct H2Settled {
    pub status: u16,
    pub headers: Box<[(Box<str>, Box<str>)]>,
    pub bytes: usize,
    /// Whether the stream was reset under its committed head rather than ended.
    pub reset: bool,
    pub trailers: Box<[(Box<str>, Box<str>)]>,
}

impl H2Settled {
    /// The first value one trailer name carries, if the peer sent it.
    pub fn trailer(&self, name: &str) -> Option<&str> {
        pair_value(&self.trailers, name)
    }

    /// The first value one response-header name carries, if the peer sent it.
    pub fn header(&self, name: &str) -> Option<&str> {
        pair_value(&self.headers, name)
    }
}

impl H2ReadHalf {
    /// This exchange's committed status.
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// Read what is left of this download: its payload, its end, and its
    /// trailers.
    ///
    /// A reset under an answered head is this half's answer rather than a
    /// fault: the status is already on the wire and no later owner can replace
    /// it. Trailers are read only for a stream that ended, because a reset
    /// stream has none to carry.
    pub async fn settle(mut self) -> H2Settled {
        let deadline = Instant::now() + self.bound;
        let mut bytes = 0;
        let ended = drain_body(&mut self.body, deadline, "HTTP/2 answer body", |chunk| {
            bytes += chunk.len();
        })
        .await;
        let reset = ended_in_reset(ended, "HTTP/2 answer body", bytes);
        let trailers = match reset {
            true => Box::default(),
            false => bounded(self.body.trailers(), remaining(deadline), "HTTP/2 trailers")
                .await
                .ok()
                .flatten()
                .map_or_else(Box::default, |trailers| header_pairs(&trailers)),
        };
        H2Settled {
            status: self.status,
            headers: self.headers,
            bytes,
            reset,
            trailers,
        }
    }
}

/// Read one header map into the owned pairs every answer here carries.
fn header_pairs(headers: &::http::HeaderMap) -> Box<[(Box<str>, Box<str>)]> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                Box::from(name.as_str()),
                Box::from(String::from_utf8_lossy(value.as_bytes()).as_ref()),
            )
        })
        .collect()
}

/// The first value one name carries in a set of wire pairs.
fn pair_value<'a>(pairs: &'a [(Box<str>, Box<str>)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_ref())
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
        Credit::Granted => send_frame(stream, frame, false).await,
        Credit::Closed => FramePush::PeerStopped,
        Credit::Failed(error) => FramePush::Failed(error),
        Credit::Withheld => {
            panic!("HTTP/2 request flow-control capacity timed out after {bound:?}")
        }
    }
}

/// Send one whole request body, ending the stream with it.
///
/// An empty body was ended by the head that declared it, so there is nothing
/// left to send. A peer that refused from the head alone has already reset this
/// stream, and its answer is what the caller reads either way; only a send that
/// failed for some other reason is this client's fault.
async fn send_whole_body(stream: &mut h2::SendStream<Bytes>, body: &[u8]) {
    if body.is_empty() {
        return;
    }
    match send_frame(stream, body, true).await {
        FramePush::Accepted | FramePush::PeerStopped => {}
        FramePush::Failed(error) => panic!("the HTTP/2 request body could not be sent: {error}"),
    }
}

/// Send one frame after the peer grants enough flow-control capacity.
///
/// `end_of_stream` states whether this frame completes the request body, which
/// is the difference between one frame of a paced upload and a whole request
/// sent at once.
async fn send_frame(
    stream: &mut h2::SendStream<Bytes>,
    frame: &[u8],
    end_of_stream: bool,
) -> FramePush {
    match stream.send_data(Bytes::copy_from_slice(frame), end_of_stream) {
        Ok(()) => FramePush::Accepted,
        Err(error) => push_failure(stream, error).await,
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
///
/// A frame offered to a stream the peer has already ended comes back as the
/// user error `InactiveStreamId`, which carries no reason of its own: by its
/// type alone, a peer that refused this request and a misuse of this client
/// look the same. So the stream is asked instead of the error, and that is not
/// a race this side loses — a server answering from the head alone resets the
/// request body it will never read, and the sender learns of that reset only
/// once its own frame is refused.
async fn push_failure(stream: &mut h2::SendStream<Bytes>, error: h2::Error) -> FramePush {
    match is_graceful_end(&error) || peer_ended_the_stream(stream).await {
        true => FramePush::PeerStopped,
        false => FramePush::Failed(error),
    }
}

/// Whether the peer has already ended this stream gracefully.
///
/// Polled once rather than awaited: what this asks is the disposition the
/// stream carries now, and awaiting would park the caller on a stream that is
/// still open until the peer got around to resetting it.
async fn peer_ended_the_stream(stream: &mut h2::SendStream<Bytes>) -> bool {
    let reset = std::future::poll_fn(|cx| Poll::Ready(stream.poll_reset(cx))).await;
    matches!(reset, Poll::Ready(Ok(reason)) if reason == h2::Reason::NO_ERROR)
}

/// Read one HTTP/2 response body frame by frame, and report what it ended on.
///
/// Capacity is released per frame because the window is the sender's budget: a
/// reader that took the bytes and never released it would stall the peer partway
/// through a body, and the case waiting on that body would report a timeout for
/// a server that was doing exactly what it was told.
///
/// `deadline` covers the whole body, not one frame of it. A per-frame budget
/// gives a peer dribbling frames a fresh full bound for each one, so a body that
/// never ends outlasts its caller's deadline by as many frames as the peer cares
/// to send.
///
/// The ending is handed back rather than judged here, because the callers do not
/// agree on what it means: a reader of a committed answer takes a reset as its
/// row's answer, and a reader of a whole body takes everything but a graceful
/// end as a fault. Each frame goes to `received`, so a caller that wants the
/// payload keeps it and a caller that wants only its size counts it.
async fn drain_body(
    body: &mut h2::RecvStream,
    deadline: Instant,
    operation: &str,
    mut received: impl FnMut(&[u8]),
) -> Result<(), h2::Error> {
    while let Some(chunk) = bounded(body.data(), remaining(deadline), operation).await {
        let chunk = chunk?;
        body.flow_control()
            .release_capacity(chunk.len())
            .expect("the HTTP/2 reader could not release its flow-control capacity");
        received(&chunk);
    }
    Ok(())
}

/// Read one committed answer body's ending as one of the three it can have.
///
/// A reset under an answered head is the stream-local disposition a post-commit
/// terminal applies, and it is exactly what the rows asserting on a reset name.
/// Every other ending is the connection collapsing under the case — a GOAWAY,
/// the socket beneath it, or this client's own aborted driver — so the two are
/// kept apart rather than merged into one failure flag: a row that recorded a
/// collapse as a reset would pass for a reason it does not claim.
fn body_end(ended: Result<(), h2::Error>) -> H2BodyEnd {
    match ended {
        Ok(()) => H2BodyEnd::Ended,
        Err(error) if error.is_reset() => H2BodyEnd::Reset,
        Err(error) => H2BodyEnd::Collapsed(error.to_string().into_boxed_str()),
    }
}

/// Read one committed answer body's ending as a reset or as a fault.
///
/// For the readers whose row has no claim on a lost connection: only the reset
/// is an outcome there, and a collapse is the case failing under them.
fn ended_in_reset(ended: Result<(), h2::Error>, operation: &str, delivered: usize) -> bool {
    match body_end(ended) {
        H2BodyEnd::Ended => false,
        H2BodyEnd::Reset => true,
        H2BodyEnd::Collapsed(failure) => {
            panic!("the {operation} failed after {delivered} bytes: {failure}")
        }
    }
}

/// Read one HTTP/2 response body to end of stream, keeping its payload.
///
/// A peer that resets this stream with `NO_ERROR` after answering has ended it
/// gracefully — the stream-local disposition a refusal applies to the request
/// body it will never read. Any other ending is a fault, because this caller
/// asked for a whole body and did not get one.
pub async fn drain_h2_body(
    mut body: h2::RecvStream,
    operation: &str,
    bound: Duration,
) -> Box<[u8]> {
    let mut bytes = Vec::new();
    let ended = drain_body(&mut body, Instant::now() + bound, operation, |chunk| {
        bytes.extend_from_slice(chunk)
    })
    .await;
    match ended {
        Ok(()) => {}
        Err(error) if is_graceful_end(&error) => {}
        Err(error) => panic!("an HTTP/2 body frame failed: {error}"),
    }
    bytes.into_boxed_slice()
}

/// End the connection driver and read what it ended as.
///
/// The response is already complete, so the driver is aborted rather than waited
/// on — the server's own keepalive policy decides when it would otherwise close.
/// A case whose claim is that its peer saw the frames it queued cannot use this;
/// [`PersistentH2Client::close_settled`] is the close that lets them out.
async fn join_driver(driver: tokio::task::JoinHandle<Result<(), h2::Error>>) {
    driver.abort();
    report_driver(driver.await);
}

/// Report the four outcomes one joined driver can have.
///
/// Shared by the aborted close and the settled one, so the two teardown paths
/// cannot disagree about which of them is a fault. Only a clean end and the
/// driver's own cancellation are accepted: a protocol failure or a panic inside
/// it is a fault, and a join that only checked for cancellation would discard
/// both.
fn report_driver(joined: Result<Result<(), h2::Error>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => {}
        Err(error) if error.is_cancelled() => {}
        Ok(Err(error)) => panic!("HTTP/2 client driver failed: {error}"),
        Err(error) => panic!("HTTP/2 client driver join failed: {error}"),
    }
}

/// How one committed HTTP/2 body ended.
///
/// The three endings are not interchangeable, and which one a post-commit row
/// got is the row's whole claim: a reset ends one stream while the connection
/// beneath it keeps answering, a collapse takes that connection away, and a
/// clean end means no terminal fired at all. The collapse carries what the
/// transport reported, because a row that accepts one still has to say so with
/// the failure in hand.
#[derive(Debug, Eq, PartialEq)]
pub enum H2BodyEnd {
    /// End of stream: the whole body arrived.
    Ended,
    /// This stream alone was reset under its committed head.
    Reset,
    /// The connection beneath this stream went away.
    Collapsed(Box<str>),
}

/// What one HTTP/2 peer saw of a committed streaming response.
#[derive(Debug, Eq, PartialEq)]
pub struct H2Streamed {
    pub status: u16,
    /// Payload bytes the peer actually received.
    pub bytes: usize,
    /// How the body under the committed head ended.
    pub end: H2BodyEnd,
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

    /// Read this download until its body ends, however it ends.
    ///
    /// The head is read first when a case has not already read it, so one call
    /// covers the rows whose cause is the producer and the rows whose cause is
    /// something the case does between the head and the body.
    ///
    /// The ending is reported rather than judged, because a caller that stops
    /// the server under its own download is entitled to lose the connection
    /// while a caller whose producer failed is not.
    pub async fn drain(&mut self) -> H2Streamed {
        if self.response.is_some() {
            self.head().await;
        }
        let deadline = Instant::now() + self.bound;
        let mut body = self
            .body
            .take()
            .expect("this HTTP/2 download's body was already drained");
        let ended = drain_body(&mut body, deadline, "HTTP/2 download body", |chunk| {
            self.bytes += chunk.len();
        })
        .await;
        H2Streamed {
            status: self.status,
            bytes: self.bytes,
            end: body_end(ended),
        }
    }

    /// Cancel this stream, leaving the body it was given unread.
    pub fn reset(&mut self) {
        self.stream.send_reset(h2::Reason::CANCEL);
    }
}
