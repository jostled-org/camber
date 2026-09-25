//! Generated client frame sequences, each sent on a real upgraded socket.
//!
//! One family generates valid sequences: masked text and binary messages,
//! fragmented messages with control frames between their fragments, pings,
//! unsolicited pongs, and a closing handshake. The callback must receive the
//! exact application bytes, the transport must answer each ping with its own
//! payload, and the close must complete. The other family generates one
//! protocol fault per row: a clear mask bit, a reserved bit, a reserved opcode,
//! an oversized or fragmented control frame, a broken fragment sequence, or
//! text that is not UTF-8. Nothing from the faulting frame onward may reach the
//! callback, and the transport must end within the read bound.
//!
//! Rows assert the closure, not its cause. Which terminal cause the bridge
//! commits and how long a permit lives are the subject of the lifecycle and
//! shared-payload suites.

#![cfg(feature = "ws")]

use crate::common::{self, RawFrame};
use crate::deterministic::{DeterministicCase, Family};
use crate::handshake::{assert_transport_ends, perform_raw_ws_handshake};
use bytes::Bytes;
use camber::RuntimeError;
use camber::http::{Request, Router, WsConn, WsMessage};
use camber::runtime;
use std::collections::BTreeSet;
use std::io;
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// The route every generated row upgrades.
const SOCKET: &str = "/frames";

/// Live rows each family sends against the one ready server.
const CASES_PER_FAMILY: u64 = 24;

/// The most payload bytes one row sends, across all its frames.
const ROW_PAYLOAD_BUDGET: usize = 64 * 1024;

/// How long a row waits for the callback to report what it received.
const LOG_BOUND: Duration = Duration::from_secs(5);

/// The most frames a faulted transport may send before it ends.
const FRAMES_AFTER_FAULT: usize = 4;

const VALID_SEQUENCE_SEED: u64 = 0x5746_5641_4c49_4401;
const PROTOCOL_FAULT_SEED: u64 = 0x5746_4641_554c_5402;

const CONTINUATION: u8 = 0x0;
const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xa;

/// Characters valid text is built from: one to four UTF-8 bytes each, with the
/// edges of every encoded width.
const TEXT_CHARACTERS: &[char] = &[
    'a',
    'Z',
    '7',
    ' ',
    '~',
    '\u{7f}',
    '\u{80}',
    'é',
    '\u{7ff}',
    '\u{800}',
    '€',
    '\u{fffd}',
    '\u{10000}',
    '𝄞',
    '\u{10ffff}',
];

/// Byte sequences no UTF-8 decoder may accept, and whether the sequence is
/// only invalid because the message ends inside it.
const INVALID_UTF8: &[(&[u8], bool)] = &[
    (&[0xff], false),
    (&[0xc0, 0xaf], false),
    (&[0xed, 0xa0, 0x80], false),
    (&[0xf4, 0x90, 0x80, 0x80], false),
    (&[0x80], false),
    (&[0xe2, 0x82], true),
];

/// Close codes a peer may send, each with the reason it carries.
const CLOSE_STATUSES: &[(u16, &str)] =
    &[(1000, ""), (1001, "going away"), (3000, "app"), (4999, "é")];

/// The coverage the valid family must reach across its rows.
const VALID_LABELS: &[&str] = &[
    "text",
    "binary",
    "empty message",
    "fragmented",
    "code point split",
    "ping",
    "pong",
    "interleaved ping",
    "interleaved pong",
    "16-bit length",
    "64-bit length",
    "close status",
];

/// One application message, as the callback received it.
#[derive(Debug, Eq, PartialEq)]
enum Received {
    Text(Box<str>),
    Binary(Bytes),
}

impl Received {
    /// The application bytes the message carries.
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Binary(data) => data,
        }
    }
}

impl From<WsMessage> for Received {
    fn from(message: WsMessage) -> Self {
        match message {
            WsMessage::Text(text) => Self::Text(text),
            WsMessage::Binary(data) => Self::Binary(data),
        }
    }
}

/// One thing a row does on the wire.
#[derive(Debug, Eq, PartialEq)]
enum Step {
    /// Write a frame the server must accept.
    Send(Box<[u8]>),
    /// Write a frame the server must refuse. Every write from here on may find
    /// the transport already gone.
    Fault(Box<[u8]>),
    /// Read the pong that answers a ping, carrying this payload.
    Pong(Box<[u8]>),
}

/// How a row ends.
#[derive(Debug, Eq, PartialEq)]
enum Ending {
    /// Send this close payload, read the server's close, then the transport's
    /// end.
    CloseHandshake(Box<[u8]>),
    /// Read at most a close frame, then the transport's end.
    ProtocolFailure,
}

/// One generated row, stated in full, and what it must earn.
#[derive(Debug, Eq, PartialEq)]
struct Row {
    category: &'static str,
    labels: Box<[&'static str]>,
    steps: Box<[Step]>,
    delivered: Box<[Received]>,
    ending: Ending,
}

/// A row under construction.
///
/// Collects the steps, the messages the callback must receive, and the
/// coverage labels the row earned, then seals all three.
struct Script<'c> {
    case: &'c mut DeterministicCase,
    steps: Vec<Step>,
    delivered: Vec<Received>,
    labels: BTreeSet<&'static str>,
    budget: usize,
}

impl<'c> Script<'c> {
    fn new(case: &'c mut DeterministicCase) -> Self {
        Self {
            case,
            steps: Vec::new(),
            delivered: Vec::new(),
            labels: BTreeSet::new(),
            budget: ROW_PAYLOAD_BUDGET,
        }
    }

    /// A masked frame under a generated key.
    fn masked(&mut self, frame: RawFrame<'_>) -> Box<[u8]> {
        let key = [0; 4].map(|_: u8| byte(self.case));
        self.label_length(frame.payload.len());
        RawFrame {
            mask: Some(key),
            ..frame
        }
        .encode()
    }

    fn label_length(&mut self, length: usize) {
        match length {
            0..=125 => {}
            126..=65535 => {
                self.labels.insert("16-bit length");
            }
            _ => {
                self.labels.insert("64-bit length");
            }
        }
    }

    fn send(&mut self, frame: RawFrame<'_>) {
        let bytes = self.masked(frame);
        self.steps.push(Step::Send(bytes));
    }

    fn fault(&mut self, bytes: Box<[u8]>) {
        self.steps.push(Step::Fault(bytes));
    }

    fn masked_fault(&mut self, frame: RawFrame<'_>) {
        let bytes = self.masked(frame);
        self.fault(bytes);
    }

    /// A payload length drawn from a size class, within what the row has left.
    fn payload_length(&mut self) -> usize {
        let wanted = match self.case.below(4) {
            0 => 0,
            1 => 1 + self.case.below(125),
            2 => 126 + self.case.below(2048),
            _ => ROW_PAYLOAD_BUDGET,
        };
        let length = wanted.min(self.budget);
        self.budget -= length;
        length
    }

    fn text(&mut self, length: usize) -> Box<str> {
        let mut text = String::with_capacity(length);
        loop {
            let next = *self.case.pick(TEXT_CHARACTERS);
            match text.len() + next.len_utf8() <= length {
                true => text.push(next),
                false => return text.into_boxed_str(),
            }
        }
    }

    fn binary(&mut self, length: usize) -> Box<[u8]> {
        (0..length).map(|_| byte(self.case)).collect()
    }

    /// A ping and the pong that must answer it.
    fn ping(&mut self) {
        let length = self.case.below(126);
        let payload = self.binary(length);
        self.send(RawFrame::complete(PING, &payload));
        self.steps.push(Step::Pong(payload));
    }

    /// A pong nobody asked for, which the transport must absorb.
    fn pong(&mut self) {
        let length = self.case.below(126);
        let payload = self.binary(length);
        self.send(RawFrame::complete(PONG, &payload));
    }

    /// One whole message, in one to three fragments, with a control frame
    /// sometimes sent between two of them.
    fn message(&mut self) {
        let length = self.payload_length();
        let (opcode, message) = match self.case.boolean() {
            true => {
                self.labels.insert("text");
                (TEXT, Received::Text(self.text(length)))
            }
            false => {
                self.labels.insert("binary");
                (BINARY, Received::Binary(Bytes::from(self.binary(length))))
            }
        };
        let payload = message.as_bytes();
        if payload.is_empty() {
            self.labels.insert("empty message");
        }
        let splits = self.splits(payload.len());
        self.fragments(opcode, payload, &splits);
        self.delivered.push(message);
    }

    /// Zero to two sorted fragment boundaries inside `length` bytes. A text
    /// boundary may fall inside a code point: UTF-8 validity belongs to the
    /// whole message, not to each fragment.
    fn splits(&mut self, length: usize) -> Box<[usize]> {
        let mut splits: Vec<usize> = (0..self.case.below(3))
            .map(|_| self.case.below(length + 1))
            .collect();
        splits.sort_unstable();
        splits.into_boxed_slice()
    }

    /// Send `payload` as one message cut at `splits`.
    fn fragments(&mut self, opcode: u8, payload: &[u8], splits: &[usize]) {
        let bounds: Box<[usize]> = std::iter::once(0)
            .chain(splits.iter().copied())
            .chain(std::iter::once(payload.len()))
            .collect();
        if bounds.len() > 2 {
            self.labels.insert("fragmented");
        }
        let text = (opcode == TEXT)
            .then(|| std::str::from_utf8(payload))
            .and_then(Result::ok);
        if text.is_some_and(|text| splits.iter().any(|split| !text.is_char_boundary(*split))) {
            self.labels.insert("code point split");
        }
        let last = bounds.len() - 2;
        bounds.windows(2).enumerate().for_each(|(index, window)| {
            let frame = RawFrame {
                fin: index == last,
                opcode: match index {
                    0 => opcode,
                    _ => CONTINUATION,
                },
                ..RawFrame::complete(opcode, &payload[window[0]..window[1]])
            };
            self.send(frame);
            if index < last {
                self.interleave();
            }
        });
    }

    /// Sometimes send one control frame between two fragments.
    fn interleave(&mut self) {
        match self.case.below(3) {
            0 => {
                self.labels.insert("interleaved ping");
                self.ping();
            }
            1 => {
                self.labels.insert("interleaved pong");
                self.pong();
            }
            _ => {}
        }
    }

    /// The close payload a valid row ends with.
    fn close_payload(&mut self) -> Box<[u8]> {
        match self.case.boolean() {
            true => {
                self.labels.insert("close status");
                let (code, reason) = *self.case.pick(CLOSE_STATUSES);
                code.to_be_bytes()
                    .into_iter()
                    .chain(reason.bytes())
                    .collect()
            }
            false => Box::default(),
        }
    }

    fn seal(self, category: &'static str, ending: Ending) -> Row {
        Row {
            category,
            labels: self.labels.into_iter().collect(),
            steps: self.steps.into_boxed_slice(),
            delivered: self.delivered.into_boxed_slice(),
            ending,
        }
    }
}

const FAMILIES: [Family<Row>; 2] = [
    Family {
        name: "valid sequences",
        seed: VALID_SEQUENCE_SEED,
        generate: valid_sequence,
    },
    Family {
        name: "protocol faults",
        seed: PROTOCOL_FAULT_SEED,
        generate: protocol_fault,
    },
];

#[test]
fn generated_websocket_frames_accept_only_valid_sequences() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (log, logged) = mpsc::channel();
            let addr = common::spawn_server(recording_router(log));
            let coverage: Box<[BTreeSet<&str>]> = FAMILIES
                .iter()
                .map(|family| run_family(addr, family, &logged))
                .collect();

            let valid: BTreeSet<&str> = VALID_LABELS.iter().copied().collect();
            assert_eq!(
                coverage[0], valid,
                "the valid family reached every dimension"
            );
            assert!(
                coverage[1].contains(VALID_PREFIX),
                "some fault followed a delivered message: {:?}",
                coverage[1]
            );
            assert_eq!(
                coverage[1].len(),
                FAULTS.len() + 1,
                "every fault category ran: {:?}",
                coverage[1]
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

/// Run every case of one family, and return the coverage its rows reached.
fn run_family(
    addr: SocketAddr,
    family: &Family<Row>,
    logged: &mpsc::Receiver<Box<[Received]>>,
) -> BTreeSet<&'static str> {
    family
        .rows(CASES_PER_FAMILY)
        .flat_map(|(case, row)| {
            let context = format!(
                "{case} family={} category={} labels={:?}",
                family.name, row.category, row.labels
            );
            run_row(addr, &row, logged, &context);
            std::iter::once(row.category).chain(row.labels)
        })
        .filter(|label| *label != "valid sequence")
        .collect()
}

/// Upgrade, play one row, end it, and compare what the callback received.
fn run_row(addr: SocketAddr, row: &Row, logged: &mpsc::Receiver<Box<[Received]>>, context: &str) {
    let (mut stream, head) = perform_raw_ws_handshake(addr, &common::ws_upgrade_request(SOCKET));
    assert_eq!(
        head.status, 101,
        "{context}: the upgrade was refused: {head:?}"
    );
    let faulted = play(&mut stream, &row.steps, context);
    match &row.ending {
        Ending::CloseHandshake(payload) => close_handshake(&mut stream, payload, context),
        Ending::ProtocolFailure => {
            assert!(faulted, "{context}: a failure row sent no fault");
            protocol_failure(&mut stream, context);
        }
    }
    drop(stream);
    let received = logged
        .recv_timeout(LOG_BOUND)
        .unwrap_or_else(|error| panic!("{context}: the callback never returned: {error}"));
    assert_eq!(
        *received, *row.delivered,
        "{context}: the callback received other application messages"
    );
}

/// Play every step, and report whether any of them was a fault.
fn play(stream: &mut TcpStream, steps: &[Step], context: &str) -> bool {
    steps.iter().fold(false, |faulted, step| match step {
        Step::Send(frame) => {
            write(stream, frame, faulted, context);
            faulted
        }
        Step::Fault(frame) => {
            write(stream, frame, true, context);
            true
        }
        Step::Pong(payload) => {
            let (opcode, answer) = common::try_read_ws_frame_raw(stream)
                .unwrap_or_else(|error| panic!("{context}: no pong answered a ping: {error}"));
            assert_eq!(
                (opcode, answer.as_ref()),
                (PONG, payload.as_ref()),
                "{context}: the ping was not answered with its own payload"
            );
            faulted
        }
    })
}

/// Write one frame. Once a fault is on the wire, a closed peer is an accepted
/// answer to the write.
fn write(stream: &mut TcpStream, frame: &[u8], faulted: bool, context: &str) {
    match common::try_write_raw_frame(stream, frame) {
        Ok(()) => {}
        Err(error) if faulted && common::is_closed_connection_error(&error) => {}
        Err(error) => panic!("{context}: a frame could not be sent: {error}"),
    }
}

/// Send a close, read the server's close carrying the same status, then the
/// end of the transport.
fn close_handshake(stream: &mut TcpStream, payload: &[u8], context: &str) {
    write(stream, &masked_close(payload), false, context);
    let (opcode, answer) = common::try_read_ws_frame_raw(stream)
        .unwrap_or_else(|error| panic!("{context}: no close answered the close: {error}"));
    assert_eq!(
        opcode, CLOSE,
        "{context}: expected the server's close frame"
    );
    assert_eq!(
        answer.get(..2),
        payload.get(..2),
        "{context}: the close reply did not echo the status"
    );
    assert_transport_ends(stream, context);
}

/// A faulted transport may send one close; it must then end within the read
/// bound, and it may never send an application frame.
fn protocol_failure(stream: &mut TcpStream, context: &str) {
    for _ in 0..FRAMES_AFTER_FAULT {
        match common::try_read_ws_frame_raw(stream) {
            Ok((CLOSE, _)) => common::write_ws_close_frame(stream),
            Ok((opcode, payload)) => {
                panic!("{context}: a faulted transport sent opcode {opcode:#x}: {payload:?}")
            }
            Err(error) if ended(&error) => return,
            Err(error) => panic!("{context}: the faulted transport did not end: {error}"),
        }
    }
    panic!("{context}: the faulted transport kept sending frames");
}

/// Whether a frame read failed because the transport ended.
fn ended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::UnexpectedEof || common::is_closed_connection_error(error)
}

fn masked_close(payload: &[u8]) -> Box<[u8]> {
    RawFrame {
        mask: Some([0x5a, 0xa5, 0x0f, 0xf0]),
        ..RawFrame::complete(CLOSE, payload)
    }
    .encode()
}

/// One `/frames` route that records every application message it receives,
/// then reports them once the connection ends.
fn recording_router(log: mpsc::Sender<Box<[Received]>>) -> Router {
    let mut router = Router::new();
    router.ws(SOCKET, move |_request: &Request, mut connection: WsConn| {
        let received: Box<[Received]> =
            std::iter::from_fn(|| connection.recv_message().map(Received::from)).collect();
        log.send(received).map_err(|_| RuntimeError::ChannelClosed)
    });
    router
}

/// A valid sequence: one to four messages or control frames, then a close.
fn valid_sequence(_: u64, case: &mut DeterministicCase) -> Row {
    let mut script = Script::new(case);
    (0..=script.case.below(4)).for_each(|_| match script.case.below(4) {
        0 | 1 => script.message(),
        2 => {
            script.labels.insert("ping");
            script.ping();
        }
        _ => {
            script.labels.insert("pong");
            script.pong();
        }
    });
    let close = script.close_payload();
    script.seal("valid sequence", Ending::CloseHandshake(close))
}

/// The label a fault row earns when a valid message precedes its fault.
const VALID_PREFIX: &str = "valid prefix";

/// One protocol fault, sometimes after a valid message, always followed by a
/// valid message that must never arrive.
fn protocol_fault(index: u64, case: &mut DeterministicCase) -> Row {
    let mut script = Script::new(case);
    let prefixed = script.case.boolean();
    if prefixed {
        script.budget = 125;
        script.message();
    }
    let category = fault(&mut script, index);
    script.send(RawFrame::complete(TEXT, b"after the fault"));
    script.labels.clear();
    if prefixed {
        script.labels.insert(VALID_PREFIX);
    }
    script.seal(category, Ending::ProtocolFailure)
}

/// What every fault rule may draw on: a data opcode, a control opcode, and a
/// short valid payload.
struct FaultInput {
    data: u8,
    control: u8,
    short: Box<[u8]>,
}

/// One fault category: its name, and the rule that pushes its frames.
type FaultRule = (&'static str, fn(&mut Script<'_>, &FaultInput));

const FAULTS: [FaultRule; 12] = [
    ("unmasked data frame", unmasked_data),
    ("unmasked control frame", unmasked_control),
    ("reserved bit on a data frame", reserved_bit_on_data),
    (
        "reserved bit on a control or continuation frame",
        reserved_bit_on_control_or_continuation,
    ),
    ("reserved data opcode", reserved_data_opcode),
    ("reserved control opcode", reserved_control_opcode),
    ("oversized control frame", oversized_control),
    ("fragmented control frame", fragmented_control),
    ("continuation without a start", continuation_without_start),
    (
        "data frame inside a fragmented message",
        data_inside_fragmented_message,
    ),
    ("invalid UTF-8 text", invalid_utf8_text),
    (
        "invalid UTF-8 across fragments",
        invalid_utf8_across_fragments,
    ),
];

/// Push one fault onto `script`, and name its category.
fn fault(script: &mut Script<'_>, index: u64) -> &'static str {
    let data = *script.case.pick(&[TEXT, BINARY]);
    let control = *script.case.pick(&[PING, PONG]);
    let short_length = 1 + script.case.below(16);
    let short = script.text(short_length).into_boxed_bytes();
    let input = FaultInput {
        data,
        control,
        short,
    };
    let slot = usize::try_from(index).expect("a case index fits") % FAULTS.len();
    let (category, rule) = FAULTS[slot];
    rule(script, &input);
    category
}

fn unmasked_data(script: &mut Script<'_>, input: &FaultInput) {
    script.fault(RawFrame::complete(input.data, &input.short).encode());
}

fn unmasked_control(script: &mut Script<'_>, input: &FaultInput) {
    script.fault(RawFrame::complete(input.control, &input.short).encode());
}

fn reserved_bit_on_data(script: &mut Script<'_>, input: &FaultInput) {
    let frame = reserved_bits(script, RawFrame::complete(input.data, &input.short));
    script.fault(frame);
}

fn reserved_bit_on_control_or_continuation(script: &mut Script<'_>, input: &FaultInput) {
    let frame = match script.case.boolean() {
        true => RawFrame::complete(input.control, &input.short),
        false => {
            script.send(opening(input.data, &input.short));
            RawFrame::complete(CONTINUATION, &input.short)
        }
    };
    let frame = reserved_bits(script, frame);
    script.fault(frame);
}

fn reserved_data_opcode(script: &mut Script<'_>, input: &FaultInput) {
    let opcode = 0x3 + u8::try_from(script.case.below(5)).expect("opcode fits");
    script.masked_fault(RawFrame::complete(opcode, &input.short));
}

fn reserved_control_opcode(script: &mut Script<'_>, input: &FaultInput) {
    let opcode = 0xb + u8::try_from(script.case.below(5)).expect("opcode fits");
    script.masked_fault(RawFrame::complete(opcode, &input.short));
}

fn oversized_control(script: &mut Script<'_>, input: &FaultInput) {
    let oversized = 126 + script.case.below(128);
    let payload = script.binary(oversized);
    script.masked_fault(RawFrame::complete(input.control, &payload));
}

fn fragmented_control(script: &mut Script<'_>, input: &FaultInput) {
    script.masked_fault(opening(input.control, &input.short));
}

fn continuation_without_start(script: &mut Script<'_>, input: &FaultInput) {
    let fin = script.case.boolean();
    script.masked_fault(RawFrame {
        fin,
        ..RawFrame::complete(CONTINUATION, &input.short)
    });
}

fn data_inside_fragmented_message(script: &mut Script<'_>, input: &FaultInput) {
    script.send(opening(input.data, &input.short));
    script.masked_fault(RawFrame::complete(input.data, &input.short));
}

fn invalid_utf8_text(script: &mut Script<'_>, _: &FaultInput) {
    let text = invalid_text(script);
    script.masked_fault(RawFrame::complete(TEXT, &text));
}

fn invalid_utf8_across_fragments(script: &mut Script<'_>, _: &FaultInput) {
    let text = invalid_text(script);
    let split = 1 + script.case.below(text.len() - 1);
    script.send(opening(TEXT, &text[..split]));
    script.masked_fault(RawFrame::complete(CONTINUATION, &text[split..]));
}

/// The first, non-final frame of a fragmented message.
const fn opening(opcode: u8, payload: &[u8]) -> RawFrame<'_> {
    RawFrame {
        fin: false,
        ..RawFrame::complete(opcode, payload)
    }
}

/// `frame`, masked, with at least one reserved bit set.
fn reserved_bits(script: &mut Script<'_>, frame: RawFrame<'_>) -> Box<[u8]> {
    let rsv = 1 + u8::try_from(script.case.below(7)).expect("reserved bits fit");
    script.masked(RawFrame { rsv, ..frame })
}

/// ASCII text with one sequence no UTF-8 decoder accepts: anywhere for a
/// sequence that is invalid on its own, and at the end for a truncated one.
fn invalid_text(script: &mut Script<'_>) -> Box<[u8]> {
    let (sequence, truncated) = *script.case.pick(INVALID_UTF8);
    let length = 1 + script.case.below(16);
    let ascii: Box<[u8]> = (0..length).map(|_| b'a' + byte(script.case) % 26).collect();
    let at = match truncated {
        true => ascii.len(),
        false => script.case.below(ascii.len() + 1),
    };
    ascii[..at]
        .iter()
        .chain(sequence.iter())
        .chain(ascii[at..].iter())
        .copied()
        .collect()
}

fn byte(case: &mut DeterministicCase) -> u8 {
    u8::try_from(case.below(256)).expect("a generated byte fits")
}
