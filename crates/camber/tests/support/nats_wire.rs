//! The Core NATS client-protocol framing the scripted peer reads, and the
//! reply frames it writes.
//!
//! [`parse`] reads one command from the head of a buffer. A `PUB` or `HPUB`
//! is complete only when its whole payload and the closing CRLF have arrived,
//! so a partial payload is never read as commands. A frame a real server
//! would refuse is malformed, never skipped.

use std::sync::Arc;

/// The largest payload the peer advertises: above any row's maximum, so the
/// SDK's own check never answers for Camber's.
pub const ADVERTISED_MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// The longest control line the peer reads, as nats-server's default.
const MAX_CONTROL_LINE: usize = 4096;

/// The status line every header block opens with.
const HEADER_VERSION: &[u8] = b"NATS/1.0";

/// The blank line that closes a header block.
const HEADER_END: &[u8] = b"\r\n\r\n";

/// One `SUB` the peer read: subject, queue group, and subscription ID.
pub type Subscription = (Box<str>, Option<Box<str>>, Box<str>);

/// One complete `PUB` or `HPUB` the peer read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publication {
    /// The subject published to.
    pub subject: Box<str>,
    /// The reply subject, when the client named one.
    pub reply: Option<Box<str>>,
    /// The raw header block of an `HPUB`, status line and closing blank line
    /// included.
    pub headers: Option<Arc<[u8]>>,
    /// The payload, without headers.
    pub payload: Arc<[u8]>,
}

impl Publication {
    /// The first value of header `name`, matched without case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let block = std::str::from_utf8(self.headers.as_deref()?).ok()?;
        block.split("\r\n").skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

/// One command the peer read.
pub(super) enum Command {
    Connect,
    Ping,
    Pong,
    Subscribe(Subscription),
    /// `UNSUB`, with the deliveries after which the subscription ends, when
    /// the client named them.
    Unsubscribe {
        sid: Box<str>,
        max: Option<usize>,
    },
    Publish(Publication),
    /// A verb the peer does not script; read and ignored.
    Other,
}

/// What the head of a read buffer holds.
pub(super) enum Frame {
    /// Not one whole command yet.
    Incomplete,
    /// One whole command and the bytes it spans.
    Complete { command: Command, consumed: usize },
    /// A frame a real server would refuse, and why.
    Malformed(Box<str>),
}

/// Read the first command in `buffer`.
#[must_use]
pub(super) fn parse(buffer: &[u8]) -> Frame {
    let line_end = buffer.windows(2).position(|pair| pair == b"\r\n");
    if line_end.unwrap_or(buffer.len()) > MAX_CONTROL_LINE {
        return Frame::Malformed("control line too long".into());
    }
    let Some(line_end) = line_end else {
        return Frame::Incomplete;
    };
    let line = String::from_utf8_lossy(&buffer[..line_end]);
    let mut words = line.split_whitespace();
    let verb = words.next().unwrap_or_default().to_ascii_uppercase();
    let args: Box<[&str]> = words.collect();
    let body_start = line_end + 2;
    let command = match verb.as_str() {
        "PUB" => return publish(&args, 1, buffer, body_start),
        "HPUB" => return publish(&args, 2, buffer, body_start),
        "CONNECT" => Ok(Command::Connect),
        "PING" => Ok(Command::Ping),
        "PONG" => Ok(Command::Pong),
        "SUB" => subscribe(&args),
        "UNSUB" => unsubscribe(&args),
        _ => Ok(Command::Other),
    };
    match command {
        Ok(command) => Frame::Complete {
            command,
            consumed: body_start,
        },
        Err(reason) => Frame::Malformed(reason),
    }
}

/// A `SUB` with a subject, an optional queue group, and an ID.
///
/// # Errors
///
/// Any other argument count, as a server refuses it.
fn subscribe(args: &[&str]) -> Result<Command, Box<str>> {
    match args {
        [subject, sid] => Ok(Command::Subscribe(((*subject).into(), None, (*sid).into()))),
        [subject, queue, sid] => Ok(Command::Subscribe((
            (*subject).into(),
            Some((*queue).into()),
            (*sid).into(),
        ))),
        _ => Err(format!("bad subscribe line: {args:?}").into()),
    }
}

/// An `UNSUB` with an ID and an optional delivery maximum.
///
/// # Errors
///
/// A missing ID, a maximum that is not a count, or extra arguments, as a
/// server refuses them.
fn unsubscribe(args: &[&str]) -> Result<Command, Box<str>> {
    let (sid, max) = match args {
        [sid] => (*sid, None),
        [sid, max] => {
            let max = max
                .parse()
                .map_err(|error| format!("bad unsubscribe maximum {max:?}: {error}"))?;
            (*sid, Some(max))
        }
        _ => return Err(format!("bad unsubscribe line: {args:?}").into()),
    };
    Ok(Command::Unsubscribe {
        sid: sid.into(),
        max,
    })
}

/// Read a `PUB` (`lengths` 1) or `HPUB` (`lengths` 2) whose control line ends
/// at `body_start`.
fn publish(args: &[&str], lengths: usize, buffer: &[u8], body_start: usize) -> Frame {
    let Some((subject, reply, header_length, total)) = publish_line(args, lengths) else {
        return Frame::Malformed(format!("bad publish line: {args:?}").into());
    };
    let body = &buffer[body_start..];
    let Some(closing) = body.get(total..total + 2) else {
        return Frame::Incomplete;
    };
    if closing != b"\r\n" {
        return Frame::Malformed("payload not closed by CRLF".into());
    }
    let (headers, payload) = body[..total].split_at(header_length);
    let headers = match lengths {
        1 => None,
        _ if is_header_block(headers) => Some(headers.into()),
        _ => return Frame::Malformed("bad header block".into()),
    };
    Frame::Complete {
        command: Command::Publish(Publication {
            subject: subject.into(),
            reply: reply.map(Box::from),
            headers,
            payload: payload.into(),
        }),
        consumed: body_start + total + 2,
    }
}

/// Split a publish line into subject, reply, header length, and total
/// length. A `PUB` carries no headers, so its one length is the total.
fn publish_line<'a>(
    args: &[&'a str],
    lengths: usize,
) -> Option<(&'a str, Option<&'a str>, usize, usize)> {
    let (names, sizes) = args.split_at_checked(args.len().checked_sub(lengths)?)?;
    let (subject, reply) = match names {
        [subject] => (*subject, None),
        [subject, reply] => (*subject, Some(*reply)),
        _ => return None,
    };
    let sizes: Box<[usize]> = sizes
        .iter()
        .map(|size| size.parse().ok())
        .collect::<Option<_>>()?;
    let (header_length, total) = match *sizes {
        [total] => (0, total),
        [header_length, total] => (header_length, total),
        _ => return None,
    };
    (header_length <= total && total <= ADVERTISED_MAX_PAYLOAD).then_some((
        subject,
        reply,
        header_length,
        total,
    ))
}

/// Whether `headers` opens with the version and closes with a blank line.
fn is_header_block(headers: &[u8]) -> bool {
    headers.starts_with(HEADER_VERSION) && headers.ends_with(HEADER_END)
}

/// Whether `subject` matches the subscription subject `pattern`: `*` takes
/// one token, a final `>` one or more.
#[must_use]
pub(super) fn subject_matches(pattern: &str, subject: &str) -> bool {
    let mut patterns = pattern.split('.');
    let mut subjects = subject.split('.');
    loop {
        match (patterns.next(), subjects.next()) {
            (Some(">"), Some(_)) => return patterns.next().is_none(),
            (Some(expected), Some(token)) if expected == "*" || expected == token => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// A reply the peer writes to a subscription.
#[derive(Clone, Copy, Debug)]
pub enum Reply<'a> {
    /// A `MSG` carrying the payload.
    Message(&'a [u8]),
    /// An `HMSG` with only a status line, such as `503` for no responders.
    Status { code: u16, description: &'a str },
    /// An `HMSG` with a complete header block and a payload.
    Headed {
        headers: &'a [u8],
        payload: &'a [u8],
    },
}

impl Reply<'_> {
    /// Append this reply's frame for subscription `sid` on `subject`.
    pub fn encode(&self, subject: &str, sid: &str, frames: &mut Vec<u8>) {
        match *self {
            Self::Message(payload) => {
                frames.extend_from_slice(
                    format!("MSG {subject} {sid} {}\r\n", payload.len()).as_bytes(),
                );
                frames.extend_from_slice(payload);
            }
            Self::Status { code, description } => {
                let headers = status_block(code, description);
                headed(subject, sid, headers.as_bytes(), b"", frames);
            }
            Self::Headed { headers, payload } => headed(subject, sid, headers, payload, frames),
        }
        frames.extend_from_slice(b"\r\n");
    }
}

/// A header block holding only a status line.
fn status_block(code: u16, description: &str) -> String {
    match description {
        "" => format!("NATS/1.0 {code}\r\n\r\n"),
        _ => format!("NATS/1.0 {code} {description}\r\n\r\n"),
    }
}

/// Append an `HMSG` frame's line, headers, and payload.
fn headed(subject: &str, sid: &str, headers: &[u8], payload: &[u8], frames: &mut Vec<u8>) {
    let total = headers.len() + payload.len();
    frames.extend_from_slice(
        format!("HMSG {subject} {sid} {} {total}\r\n", headers.len()).as_bytes(),
    );
    frames.extend_from_slice(headers);
    frames.extend_from_slice(payload);
}
