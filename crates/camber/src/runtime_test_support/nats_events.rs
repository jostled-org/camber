//! Controlled NATS SDK events, delivered at the production consumer.

use crate::mq::nats::Connection;

/// Delivers an SDK event to a real connection's event consumer.
///
/// The event enters where the SDK's own callback hands it over, so the
/// connection's response is production's. The probe chooses the event, never
/// the response, and names no subscription: the SDK exposes none.
#[doc(hidden)]
pub struct NatsEventProbe;

impl NatsEventProbe {
    /// Deliver one slow-consumer notification to `connection`.
    pub fn deliver_slow_consumer(connection: &Connection) {
        connection.deliver_event(async_nats::Event::SlowConsumer(0));
    }
}
