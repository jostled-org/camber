//! Read one correlated reply as a JetStream publish acknowledgement.
//!
//! Classification reads the reply's status and the decoded response's typed
//! codes, never text. Only a receipt naming the configured stream with a
//! positive sequence is success. A reply Camber cannot read as a receipt or
//! an explicit refusal leaves the outcome unknown: the server may have
//! stored the message.

use crate::IntegrationError;
use crate::mq::nats::failure::{
    acknowledgement_inconclusive, acknowledgement_refused, no_stream_reached,
};
use async_nats::jetstream::publish::PublishAck;
use async_nats::jetstream::response::Response;
use async_nats::{Message, StatusCode};

/// The largest reply payload Camber decodes, in bytes.
const MAX_ACK_BYTES: usize = 4096;

/// Settle a publish by its correlated `reply`, which must name `stream`.
///
/// # Errors
///
/// `Unavailable/Safe` for no responders, the typed refusal of a decoded
/// JetStream error, and `OutcomeUnknown` for anything inconclusive.
pub(super) fn acknowledgement(reply: &Message, stream: &str) -> Result<(), IntegrationError> {
    match reply.status {
        None | Some(StatusCode::OK) => decoded(&reply.payload, stream),
        Some(StatusCode::NO_RESPONDERS) => Err(no_stream_reached()),
        Some(_) => Err(acknowledgement_inconclusive()),
    }
}

/// Decode a bounded reply payload as a receipt for `stream`.
fn decoded(payload: &[u8], stream: &str) -> Result<(), IntegrationError> {
    if payload.len() > MAX_ACK_BYTES {
        return Err(acknowledgement_inconclusive());
    }
    match serde_json::from_slice::<Response<PublishAck>>(payload) {
        Ok(Response::Ok(receipt)) if receipt.stream == stream && receipt.sequence > 0 => Ok(()),
        Ok(Response::Err { error }) => Err(acknowledgement_refused(error)),
        Ok(Response::Ok(_)) | Err(_) => Err(acknowledgement_inconclusive()),
    }
}
