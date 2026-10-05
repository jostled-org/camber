//! A read-only view of one acknowledged connection's correlation state.

use crate::mq::nats::{Acknowledgements, Connection};
use std::sync::Arc;

/// Reads the real correlation state of an acknowledged connection.
///
/// It shares that state, not the connection: it holds no access, so it never
/// keeps the connection open. It cannot insert entries, deliver replies,
/// mark submissions, or settle accounts.
#[doc(hidden)]
pub struct NatsAckProbe {
    acknowledgements: Arc<Acknowledgements>,
}

/// One reading of an acknowledged connection's correlation state.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NatsAckSnapshot {
    /// Correlation entries live now.
    pub pending: usize,
    /// The entries the correlation map has allocated room for.
    pub capacity: usize,
    /// Whether the reply receiver still routes.
    pub receiver: NatsAckReceiver,
}

/// The lifecycle of an acknowledged connection's reply receiver.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NatsAckReceiver {
    /// Replies are routed to waiting publishes.
    Routing,
    /// Routing ended; no publish registers again.
    Ended,
}

impl NatsAckProbe {
    /// Observe `connection`'s correlation state; `None` for a Core
    /// connection, which has none.
    #[must_use]
    pub fn observe(connection: &Connection) -> Option<Self> {
        connection
            .acknowledgements()
            .map(|acknowledgements| Self { acknowledgements })
    }

    /// Read the state now.
    #[must_use]
    pub fn snapshot(&self) -> NatsAckSnapshot {
        self.acknowledgements.snapshot()
    }
}
