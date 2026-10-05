//! Protocol readiness probes for local services.
//!
//! Each probe opens one connection, exchanges one protocol acknowledgement,
//! and drops the connection before it returns. A listening socket alone never
//! reads as ready.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::http::{poll_until, remaining};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};

/// The most reply bytes one probe reads.
pub const MAX_REPLY_BYTES: u64 = 64 * 1024;

const NATS_CONNECT: &[u8] = b"CONNECT {\"verbose\":false,\"pedantic\":false}\r\nPING\r\n";

/// The client every HTTP probe names. An ACME server refuses a request that
/// names none (RFC 8555, section 6.1).
const USER_AGENT: &str = "camber-local-readiness";

#[derive(Debug, thiserror::Error)]
pub enum ReadinessError {
    #[error("readiness budget is spent")]
    Expired,
    #[error("readiness transport failed: {0}")]
    Io(#[from] io::Error),
    #[error("readiness trust setup failed: {0}")]
    Trust(Box<str>),
    #[error("unexpected readiness reply: {0:?}")]
    Reply(Box<str>),
}

/// One HTTP/1.1 request and the answer that acknowledges readiness.
#[derive(Clone, Debug)]
pub struct HttpExchange {
    pub method: &'static str,
    pub path: Box<str>,
    pub content_type: Option<&'static str>,
    pub body: Box<[u8]>,
    pub status: u16,
    /// Text the reply must carry, when the status alone is not the protocol's
    /// acknowledgement.
    pub body_marker: Option<Box<str>>,
}

#[derive(Clone, Debug)]
pub enum Readiness {
    /// Core NATS: the server's `INFO`, then `CONNECT` and `PING` answered by
    /// `PONG`.
    NatsPing,
    /// One plain HTTP exchange.
    Http(HttpExchange),
    /// One HTTP exchange over TLS that trusts only `roots_pem`.
    Https {
        exchange: HttpExchange,
        server_name: Box<str>,
        roots_pem: Box<[u8]>,
    },
}

impl Readiness {
    /// An SQS query-protocol `ListQueues` answered by its response document.
    pub fn sqs_queue_query() -> Self {
        Self::Http(HttpExchange {
            method: "POST",
            path: "/".into(),
            content_type: Some("application/x-www-form-urlencoded"),
            body: b"Action=ListQueues&Version=2012-11-05".as_slice().into(),
            status: 200,
            body_marker: Some("ListQueuesResponse".into()),
        })
    }

    /// An ACME directory that advertises order creation, trusting only the
    /// fixture's own root.
    pub fn acme_directory(roots_pem: &[u8]) -> Self {
        Self::Https {
            exchange: HttpExchange {
                method: "GET",
                path: "/dir".into(),
                content_type: None,
                body: Box::default(),
                status: 200,
                body_marker: Some("newOrder".into()),
            },
            server_name: "localhost".into(),
            roots_pem: roots_pem.into(),
        }
    }

    /// The DNS challenge server's control API accepting a default address.
    pub fn challenge_control() -> Self {
        Self::Http(HttpExchange {
            method: "POST",
            path: "/set-default-ipv4".into(),
            content_type: Some("application/json"),
            body: b"{\"ip\":\"127.0.0.1\"}".as_slice().into(),
            status: 200,
            body_marker: None,
        })
    }

    /// Exchange one acknowledgement with `address` inside `budget`.
    pub fn probe(&self, address: SocketAddr, budget: Duration) -> Result<(), ReadinessError> {
        self.prepare()?.probe(address, budget)
    }

    /// Probe `address` until it acknowledges or `deadline` passes.
    ///
    /// # Errors
    ///
    /// A trust setup that cannot succeed, at once, or the last probe's
    /// refusal when `deadline` passes unacknowledged.
    pub fn await_ready(&self, address: SocketAddr, deadline: Instant) -> Result<(), Box<str>> {
        let probe = self
            .prepare()
            .map_err(|error| error.to_string().into_boxed_str())?;
        let mut last: Option<Box<str>> = None;
        let ready = poll_until(remaining(deadline), || {
            match probe.probe(address, remaining(deadline)) {
                Ok(()) => true,
                Err(error) => {
                    last = Some(error.to_string().into_boxed_str());
                    false
                }
            }
        });
        match ready {
            true => Ok(()),
            false => Err(last.unwrap_or_else(|| "no probe ran".into())),
        }
    }

    /// Build what every probe of this readiness shares, once.
    fn prepare(&self) -> Result<Probe<'_>, ReadinessError> {
        Ok(match self {
            Self::NatsPing => Probe::NatsPing,
            Self::Http(exchange) => Probe::Http(exchange),
            Self::Https {
                exchange,
                server_name,
                roots_pem,
            } => Probe::Https {
                exchange,
                server_name,
                trust: tls_trust(server_name, roots_pem)?,
            },
        })
    }
}

/// One readiness with its trust parsed, so repeated probes reuse it.
enum Probe<'a> {
    NatsPing,
    Http(&'a HttpExchange),
    Https {
        exchange: &'a HttpExchange,
        server_name: &'a str,
        trust: TlsTrust,
    },
}

/// The client configuration and peer name one TLS probe connects with.
struct TlsTrust {
    config: Arc<rustls::ClientConfig>,
    name: ServerName<'static>,
}

impl Probe<'_> {
    fn probe(&self, address: SocketAddr, budget: Duration) -> Result<(), ReadinessError> {
        if budget.is_zero() {
            return Err(ReadinessError::Expired);
        }
        let stream = TcpStream::connect_timeout(&address, budget)?;
        stream.set_read_timeout(Some(budget))?;
        stream.set_write_timeout(Some(budget))?;
        match self {
            Self::NatsPing => nats_ping(stream),
            Self::Http(exchange) => exchange.check(stream, &address.to_string()),
            Self::Https {
                exchange,
                server_name,
                trust,
            } => {
                let connection =
                    rustls::ClientConnection::new(Arc::clone(&trust.config), trust.name.clone())
                        .map_err(|error| ReadinessError::Trust(error.to_string().into()))?;
                exchange.check(rustls::StreamOwned::new(connection, stream), server_name)
            }
        }
    }
}

impl HttpExchange {
    fn check(&self, mut stream: impl Read + Write, host: &str) -> Result<(), ReadinessError> {
        let mut head = format!(
            "{} {} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: {USER_AGENT}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.method,
            self.path,
            self.body.len()
        );
        if let Some(content_type) = self.content_type {
            head.push_str("Content-Type: ");
            head.push_str(content_type);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()?;

        let reply = read_reply(&mut stream)?;
        let text = String::from_utf8_lossy(&reply);
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok());
        let marked = self
            .body_marker
            .as_deref()
            .is_none_or(|marker| text.contains(marker));
        match (status == Some(self.status), marked) {
            (true, true) => Ok(()),
            _ => Err(ReadinessError::Reply(
                text.lines().next().unwrap_or_default().into(),
            )),
        }
    }
}

fn nats_ping(stream: TcpStream) -> Result<(), ReadinessError> {
    let mut reader = BufReader::new(stream.try_clone()?.take(MAX_REPLY_BYTES));
    let info = read_line(&mut reader)?;
    if !info.starts_with("INFO ") {
        return Err(ReadinessError::Reply(info));
    }
    let mut writer = stream;
    writer.write_all(NATS_CONNECT)?;
    writer.flush()?;
    let reply = read_line(&mut reader)?;
    match &*reply {
        "PONG" => Ok(()),
        _ => Err(ReadinessError::Reply(reply)),
    }
}

fn read_line(reader: &mut impl BufRead) -> Result<Box<str>, ReadinessError> {
    let mut line = String::new();
    match reader.read_line(&mut line)? {
        0 => Err(ReadinessError::Reply("connection closed".into())),
        _ => Ok(line.trim_end_matches(['\r', '\n']).into()),
    }
}

/// Read a `Connection: close` reply to its end, bounded.
///
/// A TLS peer that closes without `close_notify` still delivered what it
/// wrote, so an unexpected end after reply bytes reads as the end.
fn read_reply(stream: &mut impl Read) -> Result<Box<[u8]>, ReadinessError> {
    let mut reply = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(reply.into_boxed_slice()),
            Ok(count) if (reply.len() + count) as u64 > MAX_REPLY_BYTES => {
                return Err(ReadinessError::Reply(
                    format!("reply exceeded {MAX_REPLY_BYTES} bytes").into(),
                ));
            }
            Ok(count) => reply.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && !reply.is_empty() => {
                return Ok(reply.into_boxed_slice());
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn tls_trust(server_name: &str, roots_pem: &[u8]) -> Result<TlsTrust, ReadinessError> {
    let trust = |error: &dyn std::fmt::Display| ReadinessError::Trust(error.to_string().into());
    let mut roots = rustls::RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(roots_pem) {
        let certificate = certificate.map_err(|error| trust(&error))?;
        roots.add(certificate).map_err(|error| trust(&error))?;
    }
    if roots.is_empty() {
        return Err(ReadinessError::Trust("no root certificate".into()));
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| trust(&error))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = ServerName::try_from(server_name.to_owned()).map_err(|error| trust(&error))?;
    Ok(TlsTrust {
        config: Arc::new(config),
        name,
    })
}
