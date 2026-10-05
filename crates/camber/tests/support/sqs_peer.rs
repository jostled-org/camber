//! A scripted SQS peer for component and acceptance rows.
//!
//! It speaks the AWS JSON 1.0 protocol the SDK sends: one HTTP/1.1 request per
//! operation, named by its `x-amz-target` header. The peer records every
//! request it reads in full, with the access key and region its signature
//! names, then answers from its script. A row scripts what a real queue cannot
//! produce on demand: a service error, a success without the field the adapter
//! needs, a batch over the adapter's bounds, a connection dropped after the
//! request was read, a request never answered, or an answer withheld until
//! the row opens a gate.
//!
//! The peer owns its listener, its acceptor thread, and one thread per
//! accepted connection. [`SqsPeer::finish`] stops and joins them all within a
//! bound; `Drop` is the fallback for an unwinding row. Every wait it offers is a
//! bounded wait for a fact the peer itself recorded, never a sleep.

use crate::integration_rows::{EXHAUSTED, Row, expect_eq};
use crate::scripted_peer::{
    PEER_POLL as POLL, PEER_UNWINDING, PeerPlan, PeerState, PeerThreads, SharedState, Spawned,
    read_timed_out, shut_down_all, spawn_tracked,
};
use camber::mq::sqs::{self, SqsBuilder};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// A bound no row reaches: work under it ends only by a row's own act.
pub const UNREACHED: Duration = Duration::from_secs(24 * 60 * 60);

/// The largest request head the peer reads.
const MAX_HEAD: usize = 64 * 1024;

/// The target prefix every SQS JSON operation carries.
const TARGET_PREFIX: &str = "AmazonSQS.";

/// The dummy access key a local builder signs with.
pub const ACCESS_KEY: &str = "camber-component";

/// The dummy secret key a local builder signs with.
pub const SECRET_KEY: &str = "camber-component-secret";

/// The region a local builder names.
pub const REGION: &str = "us-east-1";

/// How the peer answers one request it read in full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The operation's ordinary success.
    Serve,
    /// This status and JSON document.
    Json(u16, Box<str>),
    /// Close the connection without answering.
    Drop,
    /// Never answer; the connection stays open until the peer finishes.
    Hold,
    /// This status and JSON document, withheld until the row opens one more
    /// gate.
    Gated(u16, Box<str>),
}

impl Reply {
    /// A service error of `status` with the AWS JSON error `code`.
    #[must_use]
    pub fn error(status: u16, code: &str) -> Self {
        Self::Json(status, error_document(code))
    }

    /// A service error, as [`Self::error`], withheld until the row opens a
    /// gate.
    #[must_use]
    pub fn gated_error(status: u16, code: &str) -> Self {
        Self::Gated(status, error_document(code))
    }

    /// A `ReceiveMessage` success carrying one message per body.
    #[must_use]
    pub fn messages(bodies: &[&str]) -> Self {
        let messages: Vec<serde_json::Value> = bodies
            .iter()
            .enumerate()
            .map(|(index, body)| {
                serde_json::json!({
                    "MessageId": format!("peer-message-{index}"),
                    "ReceiptHandle": format!("peer-receipt-{index}"),
                    "Body": body,
                })
            })
            .collect();
        Self::Json(
            200,
            serde_json::json!({ "Messages": messages })
                .to_string()
                .into(),
        )
    }
}

/// One request the peer read in full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The operation, without its `AmazonSQS.` prefix.
    pub operation: Box<str>,
    /// The access key the request's signature names.
    pub access_key: Box<str>,
    /// The region the request's signature names.
    pub region: Box<str>,
    /// The request's JSON document; `Null` when it was malformed.
    pub body: serde_json::Value,
    /// Whether the body was not a JSON document. The peer answers such a
    /// request with a `SerializationException`, as the service does, so a
    /// malformed body fails the operation that sent it instead of reading as
    /// a document with every field absent.
    pub malformed: bool,
}

impl Recorded {
    /// A string field of the request document.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.body.get(name).and_then(serde_json::Value::as_str)
    }

    /// An integer field of the request document.
    #[must_use]
    pub fn number(&self, name: &str) -> Option<i64> {
        self.body.get(name).and_then(serde_json::Value::as_i64)
    }
}

/// What the peer has read, in order.
#[derive(Clone, Debug, Default)]
pub struct PeerLog {
    /// Connections accepted.
    pub accepted: usize,
    /// Requests read in full.
    pub requests: Vec<Recorded>,
    /// Requests the peer holds unanswered.
    pub held: usize,
    /// Gated requests read, opened or not.
    pub gated: usize,
}

impl PeerLog {
    /// Fail the row unless the peer read exactly `expected` requests, by
    /// operation, in order.
    ///
    /// # Errors
    ///
    /// The operations the peer read instead.
    pub fn expect_requests(&self, expected: &[&str]) -> Row {
        expect_eq(
            "requests the peer read",
            self.requests
                .iter()
                .map(|request| &*request.operation)
                .collect::<Vec<_>>(),
            expected.to_vec(),
        )
    }

    /// Requests of `operation`.
    #[must_use]
    pub fn count(&self, operation: &str) -> usize {
        self.requests
            .iter()
            .filter(|request| &*request.operation == operation)
            .count()
    }
}

/// The state the peer's threads and the row share.
pub struct State {
    log: PeerLog,
    /// Replies for the next requests, in order; `Serve` once empty.
    script: VecDeque<Reply>,
    stopping: bool,
    /// Writers of the connections still open.
    writers: Vec<TcpStream>,
    /// Message identities issued by `SendMessage`.
    sent: usize,
    /// Gates the row opened: gated request `n` answers once `n` are open.
    opened: usize,
}

impl PeerState for State {
    type Log = PeerLog;

    fn log(&self) -> &PeerLog {
        &self.log
    }

    fn accepted(&self) -> usize {
        self.log.accepted
    }

    fn stopping(&self) -> bool {
        self.stopping
    }

    fn stop(&mut self) {
        self.stopping = true;
        shut_down_all(self.writers.drain(..));
    }
}

type Shared = SharedState<State>;

/// Scripts and reads one SQS peer.
pub type PeerControl = crate::scripted_peer::PeerControl<State>;

/// A scripted SQS peer on an ephemeral loopback port.
///
/// The one owner of the peer's threads. A row hands [`PeerControl`] clones to
/// the code it runs and keeps the peer itself for [`Self::finish`].
pub struct SqsPeer {
    address: SocketAddr,
    threads: PeerThreads<PeerControl>,
    control: PeerControl,
}

impl SqsPeer {
    /// Bind and start answering every request with its ordinary success.
    ///
    /// # Panics
    ///
    /// When the loopback listener cannot bind: the row cannot run without it.
    #[must_use]
    pub fn start() -> Self {
        let control = PeerControl::new(State {
            log: PeerLog::default(),
            script: VecDeque::new(),
            stopping: false,
            writers: Vec::new(),
            sent: 0,
            opened: 0,
        });
        let admitting = Arc::clone(control.shared());
        let (threads, address) = PeerThreads::start(PeerPlan {
            poll: POLL,
            what: "scripted SQS peer threads",
            unwinding: PEER_UNWINDING,
            control: control.clone(),
            admit: move |stream, spawned: &Spawned| admit(stream, &admitting, spawned),
        })
        .expect("bind the scripted SQS peer");
        Self {
            address,
            threads,
            control,
        }
    }

    /// The endpoint an SDK client sends to.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    /// A queue URL on this peer.
    #[must_use]
    pub fn queue_url(&self, queue: &str) -> String {
        format!("http://{}/000000000000/{queue}", self.address)
    }

    /// A builder for this peer with dummy credentials and an explicit
    /// region, so a client built from it reads no ambient configuration.
    pub fn builder(&self) -> SqsBuilder {
        sqs::builder()
            .endpoint(&self.endpoint())
            .region(REGION)
            .credentials(ACCESS_KEY, SECRET_KEY, None)
    }

    /// A [`Self::builder`] whose operations outlive every row, so a held
    /// operation ends only when a close cuts it.
    pub fn held_until_close(&self) -> SqsBuilder {
        self.builder()
            .operation_timeout(UNREACHED)
            .shutdown_timeout(EXHAUSTED)
    }

    /// Script and read this peer.
    #[must_use]
    pub fn control(&self) -> PeerControl {
        self.control.clone()
    }

    /// Stop the peer and join its threads within `bound`.
    ///
    /// # Errors
    ///
    /// Names the threads still running after the bound.
    pub fn finish(self, bound: Duration) -> Result<(), String> {
        self.threads.finish(bound)
    }

    /// Finish the peer after a row, within `bound`, keeping the row's own
    /// verdict first.
    ///
    /// # Errors
    ///
    /// The row's failure, then the threads still running after the bound.
    pub fn finished(self, bound: Duration, verdict: Row) -> Row {
        self.threads.finished(bound, verdict)
    }
}

impl PeerControl {
    /// Answer the next requests with `replies`, in order, then `Serve`.
    pub fn script(&self, replies: impl IntoIterator<Item = Reply>) {
        self.shared().update(|state| state.script.extend(replies));
    }

    /// Open one gate: the next gated request, read or still to come, answers.
    pub fn open_gate(&self) {
        self.shared().update(|state| state.opened += 1);
    }

    /// Wait until the peer holds `count` requests unanswered, within `bound`:
    /// they were read in full, so they were submitted.
    ///
    /// # Errors
    ///
    /// The last log when the bound passes first.
    pub fn wait_held(&self, count: usize, bound: Duration) -> Result<PeerLog, String> {
        self.wait_for("the held requests", bound, |log| log.held >= count)
    }
}

fn admit(stream: TcpStream, shared: &Arc<Shared>, spawned: &Spawned) {
    let Ok(writer) = stream.try_clone() else {
        return;
    };
    let admitted = shared.update(|state| {
        state.log.accepted += 1;
        match state.stopping {
            true => false,
            false => {
                state.writers.push(writer);
                true
            }
        }
    });
    if !admitted {
        drop(stream.shutdown(Shutdown::Both));
        return;
    }
    let shared = Arc::clone(shared);
    spawn_tracked(spawned, move || serve(stream, &shared));
}

/// Serve one connection's requests until it closes or the peer stops.
fn serve(mut stream: TcpStream, shared: &Shared) {
    if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(POLL)).is_err() {
        return;
    }
    let mut buffered = Vec::new();
    while let Some(recorded) = read_request(&mut stream, &mut buffered, shared) {
        let operation = recorded.operation.clone();
        let malformed = recorded.malformed;
        let (reply, ticket) = shared.update(|state| {
            state.log.requests.push(recorded);
            let reply = match malformed {
                true => Reply::error(400, "SerializationException"),
                false => state.script.pop_front().unwrap_or(Reply::Serve),
            };
            match reply {
                Reply::Hold => state.log.held += 1,
                Reply::Gated(..) => state.log.gated += 1,
                Reply::Serve | Reply::Json(..) | Reply::Drop => {}
            }
            (reply, state.log.gated)
        });
        if matches!(reply, Reply::Gated(..)) && !gate_opened(shared, ticket) {
            return;
        }
        let answer = match reply {
            Reply::Serve => success(&operation, shared),
            Reply::Json(status, body) | Reply::Gated(status, body) => (status, body),
            Reply::Drop => {
                drop(stream.shutdown(Shutdown::Both));
                return;
            }
            Reply::Hold => {
                hold(&mut stream, shared);
                return;
            }
        };
        if write_response(&mut stream, answer.0, &answer.1).is_err() {
            return;
        }
    }
}

/// Wait until the row has opened `ticket` gates; `false` once the peer stops
/// first.
fn gate_opened(shared: &Shared, ticket: usize) -> bool {
    let mut state = shared.state();
    loop {
        match (state.stopping, state.opened >= ticket) {
            (true, _) => return false,
            (false, true) => return true,
            (false, false) => state = shared.wait_timeout(state, POLL),
        }
    }
}

/// The AWS JSON document of a service error with `code`.
fn error_document(code: &str) -> Box<str> {
    format!(r#"{{"__type":"com.amazonaws.sqs#{code}","message":"scripted"}}"#).into()
}

/// Keep the connection open, unanswered, until the peer stops or the client
/// goes away.
fn hold(stream: &mut TcpStream, shared: &Shared) {
    let mut discarded = Vec::new();
    while fill(stream, &mut discarded, shared) {
        discarded.clear();
    }
}

/// The ordinary success of `operation`.
fn success(operation: &str, shared: &Shared) -> (u16, Box<str>) {
    let body: Box<str> = match operation {
        "SendMessage" => {
            let id = shared.update(|state| {
                state.sent += 1;
                state.sent
            });
            format!(
                r#"{{"MessageId":"peer-sent-{id}","MD5OfMessageBody":"d41d8cd98f00b204e9800998ecf8427e"}}"#
            )
            .into_boxed_str()
        }
        "ReceiveMessage" => r#"{"Messages":[]}"#.into(),
        "GetQueueAttributes" => r#"{"Attributes":{}}"#.into(),
        _ => "{}".into(),
    };
    (200, body)
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} Scripted\r\ncontent-type: application/x-amz-json-1.0\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// Read the next request, or `None` once the connection or the peer ends.
fn read_request(
    stream: &mut TcpStream,
    buffered: &mut Vec<u8>,
    shared: &Shared,
) -> Option<Recorded> {
    let head_end = loop {
        if let Some(end) = find(buffered, b"\r\n\r\n") {
            break end;
        }
        if buffered.len() > MAX_HEAD || !fill(stream, buffered, shared) {
            return None;
        }
    };
    let head: Box<str> = String::from_utf8_lossy(&buffered[..head_end]).into();
    // An absent length declares no body; an unreadable one leaves no request
    // boundary to read the next request from, so the connection ends.
    let length = match header(&head, "content-length") {
        None => 0,
        Some(value) => value.parse::<usize>().ok()?,
    };
    let body_start = head_end + 4;
    let request_end = body_start + length;
    while buffered.len() < request_end {
        if !fill(stream, buffered, shared) {
            return None;
        }
    }
    let parsed = serde_json::from_slice(&buffered[body_start..request_end]);
    buffered.drain(..request_end);
    let target = header(&head, "x-amz-target").unwrap_or_default();
    let (access_key, region) = signature_scope(header(&head, "authorization").unwrap_or_default());
    Some(Recorded {
        operation: target.strip_prefix(TARGET_PREFIX).unwrap_or(target).into(),
        access_key,
        region,
        malformed: parsed.is_err(),
        body: parsed.unwrap_or(serde_json::Value::Null),
    })
}

/// Read more bytes; `false` once the connection or the peer ends.
fn fill(stream: &mut TcpStream, buffered: &mut Vec<u8>, shared: &Shared) -> bool {
    let mut chunk = [0_u8; 8192];
    loop {
        if shared.state().stopping {
            return false;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return false,
            Ok(read) => {
                buffered.extend_from_slice(&chunk[..read]);
                return true;
            }
            Err(error) if read_timed_out(&error) => {}
            Err(_) => return false,
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The value of header `name`, matched without case.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// The access key and region a SigV4 `Authorization` header's credential
/// scope names: `Credential=<key>/<date>/<region>/sqs/aws4_request`.
fn signature_scope(authorization: &str) -> (Box<str>, Box<str>) {
    let scope = authorization
        .split("Credential=")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap_or_default();
    let mut parts = scope.split('/');
    let access_key = parts.next().unwrap_or_default().into();
    let region = parts.nth(1).unwrap_or_default().into();
    (access_key, region)
}
