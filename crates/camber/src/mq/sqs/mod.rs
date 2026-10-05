//! Amazon SQS send, receive, and delete, owned by the Camber runtime.
//!
//! Every operation is async and has one spelling. A client is an admitted
//! integration instance of the runtime that connected it: it captures that
//! runtime once, runs each operation as a bounded root-scope child, and closes
//! through the runtime's settlement.
//!
//! - [`connect`] and [`SqsBuilder::connect`] validate the whole configuration
//!   before any I/O, then load configuration and credentials under the connect
//!   bound. Connecting sends no request; [`Client::ready`] queries a queue.
//! - Each operation is one request: Camber configures a single attempt and
//!   never replays one. A refusal before submission is safe to repeat. A
//!   send, receive, or delete whose answer was lost after submission returns
//!   `OutcomeUnknown`, because receive changes visibility too.
//! - A receive delivers at most ten messages and fails the whole batch when
//!   one is over the body maximum. It deletes nothing.
//! - Each operation reports one terminal: one `integration operation finished`
//!   event and one `camber_integration_operations_total` increment, labeled
//!   only by kind, operation, and outcome. A receive is one terminal for its
//!   whole batch. An admitted operation also records one
//!   `camber_integration_operation_duration_seconds` sample, from admission to
//!   its committed result; a refusal before admission records none. A repeated
//!   close read or the runtime aggregate adds no terminal.
//!
//! This is Standard-queue messaging. FIFO groups and deduplication, batch
//! APIs, and queue administration are not part of it.

mod builder;
mod client;
mod failure;
mod message;
mod submission;

pub use builder::{SqsBuilder, builder, connect};
pub use client::Client;
pub use message::Message;
