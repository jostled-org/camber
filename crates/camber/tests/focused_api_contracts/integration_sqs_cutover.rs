//! The public SQS cutover contract, entered through the crate exports.
//!
//! Type only: the owned async builder exists and can move across Tokio
//! workers. Its behavior, bounds, and wire effects belong to the component,
//! acceptance, and external roots.

use camber::mq::sqs::SqsBuilder;

use crate::integration_api::assert_send;

#[test]
fn sqs_public_cutover_contract_exists() {
    assert_send::<SqsBuilder>();
}
