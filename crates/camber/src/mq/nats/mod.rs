//! NATS messaging, owned by the Camber runtime.
//!
//! Every operation is async and has one spelling. A connection is an admitted
//! integration instance of the runtime that connected it: it captures that
//! runtime once, runs each publish and subscribe as a bounded root-scope child,
//! and closes through the runtime's settlement.
//!
//! - [`connect`] and [`NatsBuilder::connect`] validate the whole configuration
//!   before any I/O. Readiness means the server answered the connect handshake
//!   and the SDK flushed the connection.
//! - [`Connection::publish`] is Core NATS by default: it succeeds once the
//!   SDK flushed the message to its socket. That is a local flush, not a
//!   server receipt, subscriber processing, or durable storage. Publishing
//!   while disconnected returns `Unavailable`; no offline queue accumulates. A
//!   reconnect after submission discards what the SDK had not written, so that
//!   publish returns `OutcomeUnknown`, never success.
//! - [`NatsBuilder::acknowledged_publishing`] opts a connection into
//!   publishing to an existing JetStream stream. Each publish then succeeds
//!   only on a correlated server acknowledgement naming that stream. A
//!   receipt lost after submission, to a disconnect, a deadline, or an
//!   unreadable reply, is `OutcomeUnknown`. Camber creates, changes, and
//!   deletes no stream.
//! - Core NATS can drop messages for a subscriber that cannot keep up, and the
//!   SDK can drop its own overflow notifications, so Camber promises no loss
//!   detection. Each slow-consumer notification the SDK does deliver closes the
//!   whole connection and its subscriptions, and is reported as a failed close.
//!
//! - Each operation reports one terminal: one `integration operation finished`
//!   event and one `camber_integration_operations_total` increment, labeled
//!   only by kind, operation, and outcome. An admitted operation also records
//!   one `camber_integration_operation_duration_seconds` sample, from
//!   admission to its committed result; a refusal before admission records
//!   none. A repeated close read or the runtime aggregate adds no terminal.
//!
//! Subscriptions are Core NATS in both modes; there are no JetStream
//! consumers. Camber adds no retry and never republishes a message whose
//! outcome was lost.

mod ack;
mod builder;
mod connection;
mod events;
mod failure;
mod submission;
mod subscription;

pub(crate) use ack::Acknowledgements;

pub use builder::{NatsBuilder, builder, connect};
pub use connection::Connection;
pub use subscription::{Message, Subscription};
