#![cfg(feature = "sqs")]

use camber::RuntimeError;
use camber::mq::sqs;
use std::time::Duration;

#[test]
fn sqs_rejects_invalid_max_messages() {
    [0, -1, 11].into_iter().for_each(|max_messages| {
        let err = camber::__private::validate_sqs_receive(max_messages, Duration::from_secs(1))
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::MessageQueue(_)),
            "expected MessageQueue, got: {err:?}"
        );
    });
}

#[test]
fn sqs_rejects_wait_times_above_service_limit() {
    [Duration::from_secs(21), Duration::MAX]
        .into_iter()
        .for_each(|wait_time| {
            let error = camber::__private::validate_sqs_receive(1, wait_time)
                .expect_err("wait times above twenty seconds must be rejected");
            assert!(matches!(error, RuntimeError::MessageQueue(_)));
        });
}

#[tokio::test(flavor = "current_thread")]
async fn sqs_sync_facade_rejects_current_thread_runtime() {
    let error = match sqs::connect() {
        Ok(_) => panic!("sync SQS unexpectedly accepted a current-thread runtime"),
        Err(error) => error,
    };

    assert!(matches!(error, RuntimeError::MessageQueue(_)));
    assert!(error.to_string().contains("multi-thread"));
}

#[test]
fn sqs_sync_facade_rejects_missing_tokio_runtime() {
    let error = match sqs::connect() {
        Ok(_) => panic!("sync SQS unexpectedly accepted a missing Tokio runtime"),
        Err(error) => error,
    };

    assert!(matches!(error, RuntimeError::MessageQueue(_)));
    assert!(error.to_string().contains("multi-thread"));
}

#[test]
fn sqs_missing_send_message_id_is_an_error() {
    [None, Some("")].into_iter().for_each(|message_id| {
        let error = camber::__private::sqs_message_id(message_id)
            .expect_err("a successful response without a message ID is malformed");

        assert!(matches!(error, RuntimeError::MessageQueue(_)));
    });
}
