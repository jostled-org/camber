//! The public Core NATS cutover contract, entered through the crate exports.
//!
//! Type only: the owned async builder exists and can move across Tokio
//! workers. Its behavior, bounds, and wire effects belong to the component,
//! acceptance, and external roots.

use camber::mq::nats::NatsBuilder;

use crate::probes::assert_send;

#[test]
fn nats_public_cutover_contract_exists() {
    assert_send::<NatsBuilder>();
}
