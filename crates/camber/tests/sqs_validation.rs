#![cfg(feature = "sqs")]

mod common;

use camber::mq::sqs;
use camber::{RuntimeError, runtime};
use std::time::Duration;

fn make_client() -> sqs::Client {
    sqs::connect().expect("sqs::connect should succeed without real credentials")
}

#[test]
fn sqs_rejects_invalid_max_messages() {
    runtime::test(|| {
        let client = make_client();
        let queue_url = "https://sqs.us-east-1.amazonaws.com/000000000000/fake";

        // Zero: below minimum of 1
        let err = client
            .receive_messages(queue_url, 0, Duration::from_secs(1))
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::MessageQueue(_)),
            "expected MessageQueue, got: {err:?}"
        );

        // Negative
        let err = client
            .receive_messages(queue_url, -1, Duration::from_secs(1))
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::MessageQueue(_)),
            "expected MessageQueue, got: {err:?}"
        );

        // Above maximum of 10
        let err = client
            .receive_messages(queue_url, 11, Duration::from_secs(1))
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::MessageQueue(_)),
            "expected MessageQueue, got: {err:?}"
        );
    })
    .unwrap();
}
