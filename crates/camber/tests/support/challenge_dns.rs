//! The challenge DNS server of the local ACME lane.
//!
//! Pebble validates DNS-01 challenges against challtestsrv. The
//! Cloudflare-shaped peer mirrors its committed TXT store into that server
//! through its control API, so the TXT records Pebble reads are the records the
//! peer's zones hold. The oracle reads the same server over DNS: it answers
//! what Pebble would see.

use crate::dns_cleanup_peers::ZoneMirror;
use crate::resources::{lane_address, lane_variable};
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The control API's address variable, as the lane runner publishes it.
const CONTROL_ENVIRONMENT: &str = "CAMBER_LOCAL_CHALLTESTSRV_MANAGEMENT";

/// The DNS listener's address variable, as the lane runner publishes it.
const DNS_ENVIRONMENT: &str = "CAMBER_LOCAL_CHALLTESTSRV_DNS";

/// The bound on one control request or DNS query.
const REQUEST_BOUND: Duration = Duration::from_secs(10);

/// The largest DNS answer a query reads.
const MAX_DNS_REPLY: usize = 4096;

/// The fixed ID of every query; one query is in flight per connection.
const QUERY_ID: u16 = 0x4d39;

const TYPE_TXT: u16 = 16;
const CLASS_IN: u16 = 1;
const RCODE_NXDOMAIN: u8 = 3;

/// The challenge server's control API and DNS listener.
pub struct ChallengeServer {
    control: SocketAddr,
    dns: SocketAddr,
    client: reqwest::Client,
}

impl ChallengeServer {
    /// The server the lane runner started, at the addresses it published.
    ///
    /// # Errors
    ///
    /// When an address is absent or malformed, or the client cannot be built.
    pub fn from_environment() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(REQUEST_BOUND)
            .build()
            .map_err(|error| format!("challenge control client: {error}"))?;
        Ok(Self {
            control: address(CONTROL_ENVIRONMENT)?,
            dns: address(DNS_ENVIRONMENT)?,
            client,
        })
    }

    /// The control API's address.
    #[must_use]
    pub const fn control(&self) -> SocketAddr {
        self.control
    }

    /// Every TXT value the server answers for `name`, sorted.
    ///
    /// # Errors
    ///
    /// When the query fails, passes its bound, or the answer is malformed.
    pub async fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        tokio::time::timeout(REQUEST_BOUND, self.query(name))
            .await
            .map_err(|_| format!("the TXT query for {name} passed {REQUEST_BOUND:?}"))?
    }

    /// One query over TCP, each message behind its two-byte length.
    async fn query(&self, name: &str) -> Result<Vec<String>, String> {
        let mut stream = tokio::net::TcpStream::connect(self.dns)
            .await
            .map_err(|error| format!("connect to the DNS server: {error}"))?;
        let framed = framed_txt_query(name)?;
        stream
            .write_all(&framed)
            .await
            .map_err(|error| format!("send the TXT query: {error}"))?;
        let length = stream
            .read_u16()
            .await
            .map_err(|error| format!("receive the TXT answer's length: {error}"))?;
        let length = usize::from(length);
        if length > MAX_DNS_REPLY {
            return Err(format!("the TXT answer is {length} bytes"));
        }
        let mut reply = vec![0_u8; length];
        stream
            .read_exact(&mut reply)
            .await
            .map_err(|error| format!("receive the TXT answer: {error}"))?;
        txt_values(&reply)
    }

    /// Send one control request and require the server's acknowledgement.
    async fn control_request(&self, path: &str, body: serde_json::Value) -> Result<(), String> {
        let response = self
            .client
            .post(format!("http://{}{path}", self.control))
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("{path}: {error}"))?;
        match response.status() {
            reqwest::StatusCode::OK => Ok(()),
            status => Err(format!("{path} answered {status}")),
        }
    }

    async fn replace(&self, name: &str, values: &[Box<str>]) -> Result<(), String> {
        let host = format!("{name}.");
        self.control_request("/clear-txt", json!({ "host": host }))
            .await?;
        for value in values {
            self.control_request("/set-txt", json!({ "host": host, "value": value }))
                .await?;
        }
        Ok(())
    }
}

impl ZoneMirror for ChallengeServer {
    fn publish<'a>(
        &'a self,
        name: &'a str,
        values: &'a [Box<str>],
    ) -> impl Future<Output = Result<(), String>> + Send + 'a {
        self.replace(name, values)
    }
}

/// The value the lane runner published in `variable`.
///
/// # Errors
///
/// When the variable is absent or not Unicode.
pub fn environment(variable: &'static str) -> Result<String, String> {
    lane_variable(variable).map_err(|error| error.to_string())
}

/// The address the lane runner published in `variable`.
///
/// # Errors
///
/// When the variable is absent or does not parse as an address.
pub fn address(variable: &'static str) -> Result<SocketAddr, String> {
    lane_address(variable).map_err(|error| error.to_string())
}

/// The bytes before the question: the TCP length prefix and the header.
const PREAMBLE: usize = 2 + 12;

/// One recursive-desired TXT query for `name`, behind the two-byte length
/// a DNS message over TCP carries.
fn framed_txt_query(name: &str) -> Result<Vec<u8>, String> {
    // The length prefix, the header, the name's labels and root, and the
    // question's type and class.
    let mut query = Vec::with_capacity(PREAMBLE + name.len() + 2 + 4);
    query.extend_from_slice(&[0, 0]);
    query.extend_from_slice(&QUERY_ID.to_be_bytes());
    // Recursion desired; one question.
    query.extend_from_slice(&[0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
    for label in name.trim_end_matches('.').split('.') {
        let length = u8::try_from(label.len())
            .ok()
            .filter(|length| (1..=63).contains(length))
            .ok_or_else(|| format!("{name} has an invalid label"))?;
        query.push(length);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&TYPE_TXT.to_be_bytes());
    query.extend_from_slice(&CLASS_IN.to_be_bytes());
    let length = u16::try_from(query.len() - 2).map_err(|_| "the TXT query is too long")?;
    query[..2].copy_from_slice(&length.to_be_bytes());
    Ok(query)
}

/// Every TXT value in a reply to [`framed_txt_query`], sorted. A name that does not
/// exist answers none.
fn txt_values(reply: &[u8]) -> Result<Vec<String>, String> {
    let mut reader = Reader { reply, at: 0 };
    let id = reader.u16()?;
    let flags = reader.u16()?;
    let questions = reader.u16()?;
    let answers = reader.u16()?;
    reader.skip(4)?;
    let rcode = (flags & 0x000f) as u8;
    match (id, flags & 0x8000 != 0, rcode) {
        (QUERY_ID, true, 0) => {}
        (QUERY_ID, true, RCODE_NXDOMAIN) => return Ok(Vec::new()),
        _ => return Err(format!("unexpected DNS header {id:#06x} {flags:#06x}")),
    }
    for _ in 0..questions {
        reader.name()?;
        reader.skip(4)?;
    }
    let mut values = Vec::new();
    for _ in 0..answers {
        reader.name()?;
        let kind = reader.u16()?;
        reader.skip(6)?;
        let length = usize::from(reader.u16()?);
        let data = reader.take(length)?;
        if kind == TYPE_TXT {
            values.push(character_strings(data)?);
        }
    }
    values.sort();
    Ok(values)
}

/// One TXT record's character strings, joined.
fn character_strings(mut data: &[u8]) -> Result<String, String> {
    let mut value = Vec::with_capacity(data.len());
    while let Some((&length, rest)) = data.split_first() {
        let length = usize::from(length);
        let piece = rest
            .get(..length)
            .ok_or("a TXT string runs past its record")?;
        value.extend_from_slice(piece);
        data = &rest[length..];
    }
    String::from_utf8(value).map_err(|error| format!("a TXT value is not UTF-8: {error}"))
}

/// A bounds-checked cursor over one DNS reply.
struct Reader<'a> {
    reply: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(length)
            .filter(|end| *end <= self.reply.len())
            .ok_or("the DNS reply ends early")?;
        let taken = &self.reply[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn skip(&mut self, length: usize) -> Result<(), String> {
        self.take(length).map(drop)
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// Skip one name: labels ending at the root, or at a compression pointer.
    fn name(&mut self) -> Result<(), String> {
        loop {
            let length = self.take(1)?[0];
            match length {
                0 => return Ok(()),
                pointer if pointer & 0xc0 == 0xc0 => return self.skip(1),
                label => self.skip(usize::from(label))?,
            }
        }
    }
}
