//! 1.T1–1.T2: the scripted NATS peer frames publications and routes replies.
//!
//! These rows prove the fixture, not Camber. A raw loopback client writes
//! client-protocol bytes in fragments and reads the frames the peer writes
//! back. A row asserts what the peer recorded only after the peer read every
//! byte the client sent, so a partial frame is seen as partial, never raced.
//!
//! Each row owns its peer and its client. It finishes the peer on success and
//! failure alike, then proves the client's socket was closed by that finish.
#![cfg(feature = "nats")]

use crate::integration_rows::{ROW_BOUND, Row, all, expect, expect_eq, run_rows};
use crate::nats_peer::wire::{Publication, Reply};
use crate::nats_peer::{NatsPeer, PeerControl, PeerLog, Script};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Instant;

/// The header block an acknowledged publish carries.
const STREAM_HEADERS: &[u8] = b"NATS/1.0\r\nNats-Expected-Stream: ORDERS\r\nNats-Msg-Id: 7\r\n\r\n";

/// A payload that holds protocol commands the peer must not run.
const COMMAND_PAYLOAD: &[u8] = b"hi\r\nPING\r\n";

/// One frame the raw client read from the peer.
#[derive(Debug, PartialEq, Eq)]
enum ServerFrame {
    Info,
    Ping,
    Pong,
    Err(Box<str>),
    Msg {
        subject: Box<str>,
        sid: Box<str>,
        headers: Option<Box<[u8]>>,
        payload: Box<[u8]>,
    },
}

impl ServerFrame {
    /// A `MSG` with no headers.
    fn message(subject: &str, sid: &str, payload: &[u8]) -> Self {
        Self::Msg {
            subject: subject.into(),
            sid: sid.into(),
            headers: None,
            payload: payload.into(),
        }
    }
}

/// A client that speaks raw client-protocol bytes to the peer over loopback.
///
/// It owns its socket; dropping it closes the socket on every exit.
struct RawClient {
    stream: TcpStream,
    buffer: Vec<u8>,
    sent: usize,
}

impl RawClient {
    /// Connect, read the peer's `INFO`, and send `CONNECT`.
    fn connect(peer: &NatsPeer) -> Result<Self, String> {
        let stream = TcpStream::connect(peer.address())
            .map_err(|error| format!("connect to the peer: {error}"))?;
        let mut client = Self {
            stream,
            buffer: Vec::new(),
            sent: 0,
        };
        client.expect_frame("the peer's INFO", &ServerFrame::Info)?;
        client.send(b"CONNECT {\"verbose\":false,\"headers\":true}\r\n")?;
        Ok(client)
    }

    /// Write `bytes` as one fragment.
    fn send(&mut self, bytes: &[u8]) -> Row {
        self.stream
            .write_all(bytes)
            .map_err(|error| format!("send {} bytes: {error}", bytes.len()))?;
        self.sent += bytes.len();
        Ok(())
    }

    /// Wait until the peer read and framed every byte this client sent.
    fn read_by(&self, control: &PeerControl) -> Result<PeerLog, String> {
        let sent = self.sent;
        control.wait_for("every sent byte read", ROW_BOUND, |log| {
            log.received >= sent
        })
    }

    /// Fail unless the next frame the peer wrote is `expected`.
    fn expect_frame(&mut self, what: &str, expected: &ServerFrame) -> Row {
        expect_eq(what, &self.next_frame()?, expected)
    }

    /// Send `PING` and fail unless the next frame is its `PONG`: no reply
    /// was written ahead of it.
    fn expect_pong_first(&mut self, what: &str) -> Row {
        self.send(b"PING\r\n")?;
        self.expect_frame(what, &ServerFrame::Pong)
    }

    /// Read the next whole frame within [`ROW_BOUND`].
    fn next_frame(&mut self) -> Result<ServerFrame, String> {
        let deadline = Instant::now() + ROW_BOUND;
        loop {
            if let Some((frame, consumed)) = server_frame(&self.buffer)? {
                self.buffer.drain(..consumed);
                return Ok(frame);
            }
            let read = self
                .read_until(deadline)?
                .map_err(|error| format!("client read: {error}"))?;
            if read == 0 {
                return Err(format!("the peer closed mid-frame: {:?}", self.buffer));
            }
        }
    }

    /// Read once into the buffer, waiting no later than `deadline`.
    ///
    /// The outer error is a spent deadline or a socket that refused its
    /// timeout; the inner result is the read itself.
    fn read_until(&mut self, deadline: Instant) -> Result<io::Result<usize>, String> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("no whole frame within {ROW_BOUND:?}"));
        }
        self.stream
            .set_read_timeout(Some(left))
            .map_err(|error| format!("client read timeout: {error}"))?;
        let mut chunk = [0_u8; 4096];
        Ok(self
            .stream
            .read(&mut chunk)
            .inspect(|&read| self.buffer.extend_from_slice(&chunk[..read])))
    }

    /// Fail unless the peer closed or reset this socket with nothing left
    /// unread.
    fn expect_closed(mut self) -> Row {
        let deadline = Instant::now() + ROW_BOUND;
        loop {
            let read = self
                .read_until(deadline)
                .map_err(|error| format!("the peer's socket stayed open: {error}"))?;
            match read {
                Ok(0) => return expect_eq("bytes left unread", &*self.buffer, &[][..]),
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::ConnectionReset => {
                    return expect_eq("bytes left unread", &*self.buffer, &[][..]);
                }
                Err(error) => {
                    return Err(format!(
                        "the peer's socket stayed open: client read: {error}"
                    ));
                }
            }
        }
    }
}

/// Read one server frame from the head of `buffer`, with the bytes it spans.
fn server_frame(buffer: &[u8]) -> Result<Option<(ServerFrame, usize)>, String> {
    let Some(line_end) = buffer.windows(2).position(|pair| pair == b"\r\n") else {
        return Ok(None);
    };
    let line = String::from_utf8_lossy(&buffer[..line_end]);
    let words: Box<[&str]> = line.split_whitespace().collect();
    let body = line_end + 2;
    let frame = match &*words {
        ["INFO", ..] => ServerFrame::Info,
        ["PING"] => ServerFrame::Ping,
        ["PONG"] => ServerFrame::Pong,
        ["-ERR", ..] => ServerFrame::Err(line.trim_start_matches("-ERR").trim().into()),
        ["MSG", subject, sid, .., total] => {
            return delivered(buffer, body, (subject, sid), 0, total);
        }
        ["HMSG", subject, sid, .., header_length, total] => {
            let header_length = header_length
                .parse()
                .map_err(|error| format!("HMSG header length: {error}"))?;
            return delivered(buffer, body, (subject, sid), header_length, total);
        }
        _ => return Err(format!("unexpected server line: {line:?}")),
    };
    Ok(Some((frame, body)))
}

/// Read a `MSG` or `HMSG` body that starts at `body`.
fn delivered(
    buffer: &[u8],
    body: usize,
    (subject, sid): (&str, &str),
    header_length: usize,
    total: &str,
) -> Result<Option<(ServerFrame, usize)>, String> {
    let total: usize = total
        .parse()
        .map_err(|error| format!("delivery length: {error}"))?;
    let Some(frame) = buffer.get(body..body + total + 2) else {
        return Ok(None);
    };
    expect_eq("delivery closing", &frame[total..], b"\r\n".as_slice())?;
    let (headers, payload) = frame[..total]
        .split_at_checked(header_length)
        .ok_or_else(|| format!("HMSG header length {header_length} passes its total {total}"))?;
    let message = ServerFrame::Msg {
        subject: subject.into(),
        sid: sid.into(),
        headers: (header_length > 0).then(|| headers.into()),
        payload: payload.into(),
    };
    Ok(Some((message, body + total + 2)))
}

/// Finish `peer`, then prove `client`'s socket closed, keeping `verdict`
/// first.
fn torn_down(peer: NatsPeer, client: RawClient, verdict: Row) -> Row {
    let finished = peer.finish(ROW_BOUND);
    all([verdict, finished, client.expect_closed()])
}

/// Run `row` against a fresh peer and connected client, then tear both down.
fn with_client(row: impl FnOnce(&PeerControl, &mut RawClient) -> Row) -> Row {
    let peer = NatsPeer::start();
    let mut client = match RawClient::connect(&peer) {
        Ok(client) => client,
        Err(error) => return all([Err(error), peer.finish(ROW_BOUND)]),
    };
    let verdict = row(&peer.control(), &mut client);
    torn_down(peer, client, verdict)
}

/// A publication with no headers.
fn plain(subject: &str, reply: Option<&str>, payload: &[u8]) -> Publication {
    Publication {
        subject: subject.into(),
        reply: reply.map(Box::from),
        headers: None,
        payload: payload.into(),
    }
}

/// Send one `SUB` per argument list (`subject [queue] sid`) and wait until
/// the peer read them.
fn subscribe(control: &PeerControl, client: &mut RawClient, routes: &[&str]) -> Row {
    for route in routes {
        client.send(format!("SUB {route}\r\n").as_bytes())?;
    }
    client.read_by(control).map(drop)
}

/// Fail unless `control` routes `reply` on `subject` to exactly `sids`.
fn expect_routed(control: &PeerControl, subject: &str, reply: &Reply<'_>, sids: &[&str]) -> Row {
    let routed = control.reply(subject, reply)?;
    let routed: Box<[&str]> = routed.iter().map(|sid| &**sid).collect();
    expect_eq(
        &format!("subscriptions {subject} routed to"),
        &*routed,
        sids,
    )
}

// ── 1.T1 ──────────────────────────────────────────────────────────────

#[test]
fn scripted_peer_records_complete_publish_frames() {
    run_rows(&[
        (
            "a fragmented PUB is recorded only once its frame completes",
            fragmented_pub_records_once_complete,
        ),
        (
            "a fragmented HPUB keeps its headers apart from its payload",
            fragmented_hpub_keeps_headers_apart,
        ),
        (
            "a malformed payload frame is refused, not read as commands",
            malformed_payload_frame_is_refused,
        ),
        (
            "an HPUB whose header length passes its total is refused",
            oversized_header_length_is_refused,
        ),
        (
            "a SUB with the wrong argument count is refused, not ignored",
            malformed_subscribe_is_refused,
        ),
        (
            "an UNSUB with no ID or a bad maximum is refused, not ignored",
            malformed_unsubscribe_is_refused,
        ),
        (
            "a control line over the maximum is refused with its CRLF in hand",
            long_control_line_is_refused,
        ),
    ]);
}

/// The line and payload arrive in fragments; the embedded `PING` is payload.
fn fragmented_pub_records_once_complete() -> Row {
    with_client(|control, client| {
        client.send(b"PU")?;
        client.send(b"B orders.created _INBOX.r.1 10\r\nhi\r\nPI")?;
        let partial = client.read_by(control)?;
        all([
            expect_eq(
                "publications before the frame completes",
                partial.publications.len(),
                0,
            ),
            expect_eq(
                "published before the frame completes",
                partial.published.len(),
                0,
            ),
            expect_eq("pings read from a partial payload", partial.pings, 0),
        ])?;
        client.send(b"NG\r\n\r\n")?;
        client.expect_pong_first("the PONG after the frame")?;
        let log = client.read_by(control)?;
        all([
            expect_eq(
                "the recorded publication",
                &*log.publications,
                &[plain("orders.created", Some("_INBOX.r.1"), COMMAND_PAYLOAD)][..],
            ),
            expect_eq(
                "the Core publish log",
                &*log.published,
                &[("orders.created".into(), COMMAND_PAYLOAD.len())][..],
            ),
            expect_eq("pings read", log.pings, 1),
        ])
    })
}

/// Headers and payload split mid-header and mid-payload; a plain `PUB`
/// with no reply follows in the same fragment.
fn fragmented_hpub_keeps_headers_apart() -> Row {
    with_client(|control, client| {
        let total = STREAM_HEADERS.len() + 4;
        let line = format!(
            "HPUB orders.created _INBOX.r.2 {} {total}\r\n",
            STREAM_HEADERS.len()
        );
        client.send(line.as_bytes())?;
        client.send(&STREAM_HEADERS[..20])?;
        let partial = client.read_by(control)?;
        expect_eq("publications mid-header", partial.publications.len(), 0)?;
        client.send(&STREAM_HEADERS[20..])?;
        client.send(b"bo")?;
        let partial = client.read_by(control)?;
        expect_eq("publications mid-payload", partial.publications.len(), 0)?;
        client.send(b"dy\r\nPUB plain 0\r\n\r\n")?;
        let log = client.read_by(control)?;
        let headed = Publication {
            subject: "orders.created".into(),
            reply: Some("_INBOX.r.2".into()),
            headers: Some(Arc::from(STREAM_HEADERS)),
            payload: Arc::from(&b"body"[..]),
        };
        all([
            expect_eq(
                "the recorded publications",
                &*log.publications,
                &[headed, plain("plain", None, b"")][..],
            ),
            expect_eq(
                "the expected-stream header",
                log.publications
                    .first()
                    .and_then(|first| first.header("nats-expected-stream")),
                Some("ORDERS"),
            ),
            expect_eq(
                "the Core publish log",
                &*log.published,
                &[("orders.created".into(), 4), ("plain".into(), 0)][..],
            ),
        ])
    })
}

/// Bytes after a payload that are not CRLF end the connection with an
/// error; the payload's tail is never run as a `PING`.
fn malformed_payload_frame_is_refused() -> Row {
    with_client(|control, client| {
        client.send(b"PUB bad 3\r\nabcPING\r\n")?;
        let error = client.next_frame()?;
        let log = client.read_by(control)?;
        all([
            expect(
                "the peer answered -ERR",
                matches!(error, ServerFrame::Err(_)),
            ),
            expect_eq("publications", log.publications.len(), 0),
            expect_eq("pings", log.pings, 0),
            expect_eq("malformed frames", log.malformed.len(), 1),
            expect_no_open_connection(control),
        ])
    })
}

/// A header length above the total is refused before any payload is read.
fn oversized_header_length_is_refused() -> Row {
    with_client(|control, client| expect_refused(control, client, b"HPUB bad 9 4\r\n"))
}

/// A `SUB` with too few or too many arguments ends the connection.
fn malformed_subscribe_is_refused() -> Row {
    all([
        with_client(|control, client| expect_refused(control, client, b"SUB only\r\n")),
        with_client(|control, client| expect_refused(control, client, b"SUB a q 1 x\r\n")),
    ])
}

/// An `UNSUB` with no ID, a maximum that is not a count, or extra arguments
/// ends the connection.
fn malformed_unsubscribe_is_refused() -> Row {
    all([
        with_client(|control, client| expect_refused(control, client, b"UNSUB\r\n")),
        with_client(|control, client| expect_refused(control, client, b"UNSUB 1 many\r\n")),
        with_client(|control, client| expect_refused(control, client, b"UNSUB 1 2 3\r\n")),
    ])
}

/// A line past the maximum is refused even when its CRLF arrives in the
/// same read as its tail: the head is read alone first, under the maximum.
fn long_control_line_is_refused() -> Row {
    let subject = "s".repeat(4096);
    let line = format!("PUB {subject} 0\r\n\r\n");
    let (head, tail) = line.as_bytes().split_at(4000);
    with_client(|control, client| {
        client.send(head)?;
        client.read_by(control)?;
        expect_refused(control, client, tail)
    })
}

/// Send `frame` and fail unless the peer refused it with one `-ERR`,
/// recorded one malformed frame, and forgot the connection.
fn expect_refused(control: &PeerControl, client: &mut RawClient, frame: &[u8]) -> Row {
    client.send(frame)?;
    let error = client.next_frame()?;
    let log = client.read_by(control)?;
    all([
        expect(
            "the peer answered -ERR",
            matches!(error, ServerFrame::Err(_)),
        ),
        expect_eq("publications", log.publications.len(), 0),
        expect_eq("subscriptions", log.subscribed.len(), 0),
        expect_eq("unsubscriptions", log.unsubscribed.len(), 0),
        expect_eq("malformed frames", log.malformed.len(), 1),
        expect_no_open_connection(control),
    ])
}

// ── 1.T2 ──────────────────────────────────────────────────────────────

#[test]
fn scripted_peer_delays_and_routes_replies() {
    run_rows(&[
        (
            "replies route by exact and wildcard subscription",
            replies_route_by_exact_and_wildcard_subscription,
        ),
        (
            "a queue group name takes one reply across matching subjects",
            queue_group_takes_one_reply_per_name,
        ),
        (
            "success, error, and status replies keep their frames",
            reply_kinds_keep_their_frames,
        ),
        (
            "withheld replies leave protocol traffic flowing and arrive in any order",
            withheld_replies_arrive_in_any_order,
        ),
        (
            "unsubscribed and expired routes take no reply",
            unsubscribed_routes_take_no_reply,
        ),
        (
            "a closed connection takes no reply",
            closed_connection_takes_no_reply,
        ),
        (
            "a denied connection is no client close",
            denied_connection_is_no_client_close,
        ),
        (
            "a deliberately failed row still tears its peer down",
            failed_row_tears_its_peer_down,
        ),
    ]);
}

/// `*` takes one token, `>` the tail, and an exact subject only itself.
fn replies_route_by_exact_and_wildcard_subscription() -> Row {
    with_client(|control, client| {
        subscribe(
            control,
            client,
            &["_INBOX.abc.* 1", "_INBOX.exact 2", "_INBOX.abc.> 3"],
        )?;
        let body: &[u8] = b"{\"stream\":\"ORDERS\",\"seq\":1}";
        let ack = Reply::Message(body);
        expect_routed(control, "_INBOX.abc.7", &ack, &["1", "3"])?;
        client.expect_frame(
            "the wildcard reply",
            &ServerFrame::message("_INBOX.abc.7", "1", body),
        )?;
        client.expect_frame(
            "the tail-wildcard reply",
            &ServerFrame::message("_INBOX.abc.7", "3", body),
        )?;
        expect_routed(control, "_INBOX.exact", &ack, &["2"])?;
        client.expect_frame(
            "the exact reply",
            &ServerFrame::message("_INBOX.exact", "2", body),
        )?;
        expect_routed(control, "_INBOX.abc.7.8", &ack, &["3"])?;
        client.expect_frame(
            "the deep tail-wildcard reply",
            &ServerFrame::message("_INBOX.abc.7.8", "3", body),
        )?;
        all([
            expect(
                "no route for another inbox",
                control.reply("_INBOX.other.1", &ack).is_err(),
            ),
            expect(
                "no route for the bare prefix",
                control.reply("_INBOX.abc", &ack).is_err(),
            ),
            client.expect_pong_first("no stray reply"),
        ])
    })
}

/// Every matching member of one queue group name takes one reply between
/// them, whichever subject each subscribed to, as a server merges queue
/// groups by name; an ungrouped route and another group name each take their
/// own. A same-named member on a subject that does not match claims nothing.
fn queue_group_takes_one_reply_per_name() -> Row {
    with_client(|control, client| {
        subscribe(
            control,
            client,
            &[
                "_INBOX.other.* workers 1",
                "_INBOX.q.* workers 2",
                "_INBOX.q.* workers 3",
                "_INBOX.q.* 4",
                "_INBOX.> workers 5",
                "_INBOX.> audit 6",
            ],
        )?;
        let reply = Reply::Message(b"q");
        expect_routed(control, "_INBOX.q.1", &reply, &["2", "4", "6"])?;
        client.expect_frame(
            "the one workers member's reply",
            &ServerFrame::message("_INBOX.q.1", "2", b"q"),
        )?;
        client.expect_frame(
            "the ungrouped reply",
            &ServerFrame::message("_INBOX.q.1", "4", b"q"),
        )?;
        client.expect_frame(
            "the other group name's reply",
            &ServerFrame::message("_INBOX.q.1", "6", b"q"),
        )?;
        client.expect_pong_first("no second workers member's reply")?;
        expect_routed(control, "_INBOX.z.1", &reply, &["5", "6"])?;
        client.expect_frame(
            "the only matching workers member's reply",
            &ServerFrame::message("_INBOX.z.1", "5", b"q"),
        )?;
        client.expect_frame(
            "the audit reply",
            &ServerFrame::message("_INBOX.z.1", "6", b"q"),
        )?;
        client.expect_pong_first("no stray queue reply")
    })
}

/// A JetStream error body travels as `MSG`; a status travels as `HMSG`.
fn reply_kinds_keep_their_frames() -> Row {
    with_client(|control, client| {
        subscribe(control, client, &["_INBOX.k.* 9"])?;
        let error = b"{\"error\":{\"code\":400,\"err_code\":10060,\"description\":\"no\"}}";
        expect_routed(control, "_INBOX.k.1", &Reply::Message(error), &["9"])?;
        client.expect_frame(
            "the error reply",
            &ServerFrame::message("_INBOX.k.1", "9", error),
        )?;
        let status = Reply::Status {
            code: 503,
            description: "",
        };
        expect_routed(control, "_INBOX.k.2", &status, &["9"])?;
        client.expect_frame(
            "the status reply",
            &ServerFrame::Msg {
                subject: "_INBOX.k.2".into(),
                sid: "9".into(),
                headers: Some((*b"NATS/1.0 503\r\n\r\n").into()),
                payload: Box::default(),
            },
        )?;
        let headed = Reply::Headed {
            headers: b"NATS/1.0 408 Request Timeout\r\n\r\n",
            payload: b"late",
        };
        expect_routed(control, "_INBOX.k.3", &headed, &["9"])?;
        client.expect_frame(
            "the headed reply",
            &ServerFrame::Msg {
                subject: "_INBOX.k.3".into(),
                sid: "9".into(),
                headers: Some((*b"NATS/1.0 408 Request Timeout\r\n\r\n").into()),
                payload: (*b"late").into(),
            },
        )
    })
}

/// Two publications wait unanswered while `PING` still gets its `PONG`;
/// their replies then arrive newest first.
fn withheld_replies_arrive_in_any_order() -> Row {
    with_client(|control, client| {
        subscribe(control, client, &["_INBOX.r.* 1"])?;
        client.send(b"PUB orders _INBOX.r.1 1\r\na\r\nPUB orders _INBOX.r.2 1\r\nb\r\n")?;
        let publications = control.publications(2, ROW_BOUND)?;
        client.expect_pong_first("the PONG while replies are withheld")?;
        let replies: Box<[&str]> = publications
            .iter()
            .filter_map(|publication| publication.reply.as_deref())
            .collect();
        let [first, second] = *replies else {
            return Err(format!("reply subjects: {replies:?}"));
        };
        expect_routed(control, second, &Reply::Message(b"2"), &["1"])?;
        expect_routed(control, first, &Reply::Message(b"1"), &["1"])?;
        all([
            client.expect_frame(
                "the newer reply first",
                &ServerFrame::message("_INBOX.r.2", "1", b"2"),
            ),
            client.expect_frame(
                "the older reply second",
                &ServerFrame::message("_INBOX.r.1", "1", b"1"),
            ),
            expect_eq("pings read", control.log().pings, 1),
        ])
    })
}

/// `UNSUB` ends a route at once; `UNSUB` with a maximum ends it after that
/// many replies.
fn unsubscribed_routes_take_no_reply() -> Row {
    with_client(|control, client| {
        subscribe(control, client, &["_INBOX.gone.* 1", "_INBOX.once.* 2"])?;
        client.send(b"UNSUB 1\r\nUNSUB 2 1\r\n")?;
        client.read_by(control)?;
        let reply = Reply::Message(b"x");
        expect(
            "the unsubscribed route took a reply",
            control.reply("_INBOX.gone.1", &reply).is_err(),
        )?;
        expect_routed(control, "_INBOX.once.1", &reply, &["2"])?;
        client.expect_frame(
            "the one permitted reply",
            &ServerFrame::message("_INBOX.once.1", "2", b"x"),
        )?;
        all([
            expect(
                "the expired route took a second reply",
                control.reply("_INBOX.once.2", &reply).is_err(),
            ),
            client.expect_pong_first("no reply after expiry"),
        ])
    })
}

/// A connection the client closed leaves the peer's open set: a reply after
/// it finds no connection rather than writing to a dead socket.
///
/// The client closed its own socket, so only the peer's threads need tearing
/// down.
fn closed_connection_takes_no_reply() -> Row {
    let peer = NatsPeer::start();
    let verdict = close_then_reply(&peer, &peer.control());
    peer.finished(ROW_BOUND, verdict)
}

/// Subscribe, close the client, and fail unless `control` then has no open
/// connection.
fn close_then_reply(peer: &NatsPeer, control: &PeerControl) -> Row {
    let mut client = RawClient::connect(peer)?;
    subscribe(control, &mut client, &["_INBOX.c.* 1"])?;
    client
        .stream
        .shutdown(Shutdown::Both)
        .map_err(|error| format!("close the client: {error}"))?;
    control.wait_for("the client's close", ROW_BOUND, |log| log.closed >= 1)?;
    expect_no_open_connection(control)
}

/// A connection the peer denied leaves the open set before its `-ERR`
/// reaches the wire, and the peer's own end of it is no client close.
fn denied_connection_is_no_client_close() -> Row {
    let peer = NatsPeer::start();
    let control = peer.control();
    control.script(Script::DenyAuthorization);
    let verdict = deny_then_reply(&peer, &control);
    peer.finished(ROW_BOUND, verdict)
}

/// Connect under a denying script and fail unless the denial ended the
/// connection without counting a client close.
fn deny_then_reply(peer: &NatsPeer, control: &PeerControl) -> Row {
    let mut client = RawClient::connect(peer)?;
    client.expect_frame(
        "the denial",
        &ServerFrame::Err("'Authorization Violation'".into()),
    )?;
    all([
        expect_no_open_connection(control),
        expect_eq("client closes after a denial", control.log().closed, 0),
        client.expect_closed(),
    ])
}

/// Fail unless `control` has no open connection to reply on.
fn expect_no_open_connection(control: &PeerControl) -> Row {
    expect_eq(
        "a reply with no open connection",
        control.reply("_INBOX.c.1", &Reply::Message(b"x")),
        Err("no open connection".to_owned()),
    )
}

/// A row that fails on purpose still joins every peer thread and closes the
/// client's socket: teardown adds nothing to its own failure.
fn failed_row_tears_its_peer_down() -> Row {
    let deliberate = "deliberate failure";
    let outcome = with_client(|control, client| {
        subscribe(control, client, &["_INBOX.f.* 1"])?;
        Err(deliberate.to_owned())
    });
    expect_eq(
        "the failed row's verdict after teardown",
        outcome,
        Err(deliberate.to_owned()),
    )
}
