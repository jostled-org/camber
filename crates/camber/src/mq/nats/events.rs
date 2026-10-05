//! The consumer of the SDK's connection events.
//!
//! The SDK delivers events best-effort: it drops them when its own event queue
//! is full. Camber acts only on the events it receives. A slow-consumer event
//! closes the whole connection, because the SDK names no public subscription
//! the event belongs to. A closed event is the protocol closure a close waits
//! for. A disconnected event invalidates the receipts acknowledged publishes
//! still wait for.

use super::failure::failure;
use crate::integration_lifecycle::IntegrationMonitor;
use crate::{IntegrationError, IntegrationFailure, IntegrationOperation, Retryability};
use async_nats::Event;
use tokio::sync::watch;

/// The one consumer of one connection's events.
pub(super) struct EventConsumer {
    monitor: IntegrationMonitor,
    /// Set once the SDK reports its connection closed.
    closed: watch::Sender<bool>,
    /// The disconnects the SDK reported.
    disconnects: watch::Sender<()>,
}

impl EventConsumer {
    pub(super) fn new(monitor: IntegrationMonitor) -> Self {
        Self {
            monitor,
            closed: watch::Sender::new(false),
            disconnects: watch::Sender::new(()),
        }
    }

    /// Act on one delivered event.
    pub(super) fn consume(&self, event: Event) {
        match event {
            Event::SlowConsumer(_) => self.monitor.fail_and_close(slow_consumer()),
            Event::Closed => {
                self.closed.send_replace(true);
            }
            Event::Disconnected => {
                self.disconnects.send_replace(());
            }
            // Reconnection belongs to the SDK; readiness reads its state.
            Event::Connected
            | Event::LameDuckMode
            | Event::Draining
            | Event::ServerError(_)
            | Event::ClientError(_) => {}
        }
    }

    /// A view that changes with each disconnect the SDK reports from now on.
    pub(super) fn disconnects(&self) -> watch::Receiver<()> {
        self.disconnects.subscribe()
    }

    /// A view that resolves once the SDK reports its connection closed.
    pub(super) fn closed(&self) -> watch::Receiver<bool> {
        self.closed.subscribe()
    }
}

/// The failure a delivered slow-consumer event charges to the close account.
///
/// Messages were dropped for a subscriber that could not keep up. Repeating
/// the receive cannot recover them.
const fn slow_consumer() -> IntegrationError {
    failure(
        IntegrationOperation::Receive,
        IntegrationFailure::LimitExceeded,
        Retryability::Never,
    )
}
