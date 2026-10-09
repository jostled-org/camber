//! The public acknowledged-publishing contract, entered through the crate
//! exports.
//!
//! Type only: the consuming stream setter can repeat on the existing builder,
//! and an acknowledged `publish` returns exactly its Core result type, in a
//! future that moves across Tokio workers. `integration_api` owns the printed
//! acknowledged example and the builder's `Send` bound. No future here is
//! polled, so no runtime, socket, or peer exists. Validation, wire effects,
//! and acknowledgement outcomes belong to the component and acceptance roots.

use camber::RuntimeError;
use camber::mq::nats::{self, NatsBuilder};

use crate::probes::require_send;

/// Returns the publish result unconverted, so its type is the Core one.
async fn acknowledged_publish(url: &str) -> Result<(), RuntimeError> {
    let builder: NatsBuilder = nats::builder(url)
        .acknowledged_publishing("EARLIER")
        .acknowledged_publishing("EVENTS");
    let connection = builder.connect().await?;
    connection.publish("events.created", b"payload").await
}

#[test]
fn acknowledged_builder_and_publish_futures_are_send() {
    drop(require_send(acknowledged_publish("nats://127.0.0.1:4222")));
}
