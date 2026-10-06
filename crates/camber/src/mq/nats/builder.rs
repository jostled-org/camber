//! The validated configuration of one NATS connection.

use super::connection::{self, Connection};
use crate::mq::limits::{
    DEFAULT_COUNT, QueueLimits, check_setting, invalid_config, invalid_setting, validate_count,
};
use crate::{IntegrationError, IntegrationKind, RuntimeError};
use async_nats::{ServerAddr, ToServerAddrs};
use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// The longest stream name acknowledged publishing accepts, in bytes.
const MAX_STREAM_NAME: usize = 255;

/// The configuration of one NATS connection.
///
/// Setters consume the builder. Nothing is checked until [`Self::connect`],
/// which validates the whole configuration before it creates an SDK client.
/// Publishing is Core NATS unless [`Self::acknowledged_publishing`] names a
/// stream. Its `Debug` rendering replaces the userinfo of every server URL.
#[derive(Clone)]
#[must_use]
pub struct NatsBuilder {
    url: Box<str>,
    limits: QueueLimits,
    subscription_capacity: usize,
    client_capacity: usize,
    max_subscriptions: usize,
    /// The stream acknowledged publishing expects, as configured.
    acknowledged_stream: Option<Box<str>>,
}

/// A validated configuration, ready to construct an SDK client from.
pub(super) struct NatsSettings {
    pub(super) servers: Box<[ServerAddr]>,
    pub(super) limits: QueueLimits,
    pub(super) subscription_capacity: usize,
    pub(super) client_capacity: usize,
    pub(super) max_subscriptions: usize,
    /// The validated stream, when publishing is acknowledged.
    pub(super) acknowledged_stream: Option<Box<str>>,
}

/// A builder with the default bounds for the NATS server at `url`.
///
/// Pure: it needs no runtime and performs no I/O.
pub fn builder(url: &str) -> NatsBuilder {
    NatsBuilder::new(url)
}

/// Connect to the NATS server at `url` with the default bounds.
///
/// # Errors
///
/// See [`NatsBuilder::connect`].
pub async fn connect(url: &str) -> Result<Connection, RuntimeError> {
    builder(url).connect().await
}

impl NatsBuilder {
    fn new(url: &str) -> Self {
        Self {
            url: url.into(),
            limits: QueueLimits::DEFAULT,
            subscription_capacity: DEFAULT_COUNT,
            client_capacity: DEFAULT_COUNT,
            max_subscriptions: DEFAULT_COUNT,
            acknowledged_stream: None,
        }
    }

    /// Bound each publish or subscribe from admission through completion,
    /// including SDK queuing. Core operations wait for the local flush;
    /// acknowledged publishes wait for the server receipt under the same deadline.
    /// Default 30 seconds; positive and at most 24 hours.
    pub fn operation_timeout(mut self, timeout: Duration) -> Self {
        self.limits.operation_timeout = timeout;
        self
    }

    /// Bound the connect handshake and readiness flush. Default 10 seconds;
    /// positive and at most 24 hours.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.limits.connect_timeout = timeout;
        self
    }

    /// Bound the local close, further narrowed by the runtime's shutdown
    /// deadline. Default 5 seconds; positive and at most 24 hours.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.limits.shutdown_timeout = timeout;
        self
    }

    /// The most publish and subscribe operations running at once. Default 64.
    pub fn max_in_flight(mut self, max: usize) -> Self {
        self.limits.max_in_flight = max;
        self
    }

    /// The largest payload published or delivered, in bytes. Default 1 MiB.
    pub fn max_message_bytes(mut self, max: usize) -> Self {
        self.limits.max_message_bytes = max;
        self
    }

    /// The messages the SDK buffers per subscription. Default 64.
    pub fn subscription_capacity(mut self, capacity: usize) -> Self {
        self.subscription_capacity = capacity;
        self
    }

    /// The commands the SDK queues for its connection. Default 64.
    pub fn client_capacity(mut self, capacity: usize) -> Self {
        self.client_capacity = capacity;
        self
    }

    /// The most subscriptions open at once. Default 64.
    pub fn max_subscriptions(mut self, max: usize) -> Self {
        self.max_subscriptions = max;
        self
    }

    /// Publish to the existing JetStream stream `stream`: each publish
    /// succeeds only once the server acknowledges storing the message in it.
    ///
    /// Every publication carries the `Nats-Expected-Stream` header and a
    /// private reply subject. Success needs a correlated acknowledgement that
    /// names `stream` with a positive sequence: the server accepted the
    /// message under that stream's storage configuration. It promises no
    /// stronger durability, no subscriber processing, and no exactly-once
    /// delivery. Camber never retries or republishes.
    ///
    /// The stream must already exist and capture the published subjects.
    /// Connect neither looks it up nor creates, changes, or deletes it, so
    /// readiness proves only the transport and the private reply
    /// subscription. A missing stream fails its publishes; it never falls
    /// back to Core. The credentials need publish permission on the data
    /// subjects and subscribe permission on the private inbox subtree; no
    /// stream administration permission is needed.
    ///
    /// The private reply subscription is one extra SDK subscription buffering
    /// up to [`Self::subscription_capacity`] replies; it takes no slot of
    /// [`Self::max_subscriptions`]. Camber decodes at most 4096 bytes of a
    /// reply. SDK allocations for peer frames stay outside that bound.
    /// Subscriptions and receives keep their Core semantics.
    ///
    /// `stream` is 1 to 255 ASCII letters, digits, `_`, or `-`, with its case
    /// kept. A later call replaces an earlier one. Pure: [`Self::connect`]
    /// validates it before any effect.
    pub fn acknowledged_publishing(mut self, stream: &str) -> Self {
        self.acknowledged_stream = Some(stream.into());
        self
    }

    /// Validate the configuration, admit the connection to the current
    /// runtime, and connect.
    ///
    /// # Errors
    ///
    /// `InvalidConfig` for a bound out of range, an unparsable URL, or an
    /// invalid acknowledged stream name, then
    /// `NoRuntime` outside a Camber runtime, `ScopeClosed` once its admission
    /// closed, and `Busy` when its integrations are full. None of these
    /// perform I/O, and each is one connect terminal with no instance and no
    /// duration. After admission: `Unavailable`, `PermissionDenied`, or
    /// `Timeout` from the connect handshake.
    pub async fn connect(self) -> Result<Connection, RuntimeError> {
        let max_in_flight = self.limits.max_in_flight;
        crate::mq::connect::connect(
            IntegrationKind::Nats,
            self.validate(),
            max_in_flight,
            connection::establish,
        )
        .await
    }

    /// Check the whole configuration, constructing nothing.
    fn validate(self) -> Result<NatsSettings, IntegrationError> {
        let kind = IntegrationKind::Nats;
        self.limits.validate(kind)?;
        validate_count(kind, "subscription_capacity", self.subscription_capacity)?;
        validate_count(kind, "client_capacity", self.client_capacity)?;
        validate_count(kind, "max_subscriptions", self.max_subscriptions)?;
        if let Some(stream) = &self.acknowledged_stream {
            check_setting(kind, "acknowledged_publishing", valid_stream_name(stream))?;
        }
        let servers = parse_servers(&self.url)?;
        Ok(NatsSettings {
            servers,
            limits: self.limits,
            subscription_capacity: self.subscription_capacity,
            client_capacity: self.client_capacity,
            max_subscriptions: self.max_subscriptions,
            acknowledged_stream: self.acknowledged_stream,
        })
    }
}

impl fmt::Debug for NatsBuilder {
    /// Renders every server without its credentials.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NatsBuilder")
            .field("url", &RedactedServers(&self.url))
            .field("limits", &self.limits)
            .field("subscription_capacity", &self.subscription_capacity)
            .field("client_capacity", &self.client_capacity)
            .field("max_subscriptions", &self.max_subscriptions)
            .field("acknowledged_stream", &self.acknowledged_stream)
            .finish()
    }
}

/// A comma-separated server list, rendered without any URL's userinfo.
struct RedactedServers<'a>(&'a str);

impl fmt::Debug for RedactedServers<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let servers: Box<[Cow<'_, str>]> = self.0.split(',').map(without_userinfo).collect();
        fmt::Debug::fmt(&servers.join(","), f)
    }
}

/// `server` with the userinfo of its authority replaced by a marker.
fn without_userinfo(server: &str) -> Cow<'_, str> {
    let start = server.find("://").map_or(0, |at| at + "://".len());
    let (scheme, rest) = server.split_at_checked(start).unwrap_or(("", server));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    match authority.rsplit_once('@') {
        None => Cow::Borrowed(server),
        Some((_, host)) => {
            let path = rest.get(authority.len()..).unwrap_or_default();
            Cow::Owned(format!("{scheme}<redacted>@{host}{path}"))
        }
    }
}

/// Whether `stream` is 1 to 255 ASCII letters, digits, `_`, or `-`: no
/// subject token separator, wildcard, whitespace, or header delimiter.
fn valid_stream_name(stream: &str) -> bool {
    (1..=MAX_STREAM_NAME).contains(&stream.len())
        && stream
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Parse `url` as the SDK would, without resolving or connecting.
fn parse_servers(url: &str) -> Result<Box<[ServerAddr]>, IntegrationError> {
    let servers: Vec<ServerAddr> = url
        .to_server_addrs()
        .map_err(|error| invalid_config(IntegrationKind::Nats).with_source(Arc::new(error)))?
        .collect();
    match servers.is_empty() {
        true => Err(invalid_setting(IntegrationKind::Nats, "url")),
        false => Ok(servers.into_boxed_slice()),
    }
}
