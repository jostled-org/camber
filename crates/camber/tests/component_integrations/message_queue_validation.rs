#![cfg(feature = "sqs")]
//! SQS input validation and runtime capture, through the owned async API.
//!
//! Invalid receive parameters are refused before submission, a send answered
//! without a message ID is an error, and connect outside a Camber runtime is
//! `NoRuntime` on any executor, the current-thread one included. The receive
//! and message-ID claims run the 6.T1 rows that own them.

use crate::integration_rows::run_rows;
use crate::sqs_operations::{
    INVALID_BATCH_SIZES, INVALID_WAITS, missing_message_id_is_an_unknown_outcome,
    receive_refused_before_submission,
};
use crate::sqs_peer::SqsPeer;
use camber::RuntimeError;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

/// Finish `peer` and return the connections it accepted.
fn accepted_after_finish(peer: SqsPeer) -> usize {
    let accepted = peer.control().log().accepted;
    peer.finish(BOUND).expect("finish the peer");
    accepted
}

#[test]
fn sqs_rejects_invalid_max_messages() {
    run_rows(&[("batch sizes outside 1–10", || {
        receive_refused_before_submission(INVALID_BATCH_SIZES)
    })]);
}

#[test]
fn sqs_rejects_wait_times_above_service_limit() {
    run_rows(&[("waits over twenty seconds", || {
        receive_refused_before_submission(INVALID_WAITS)
    })]);
}

#[test]
fn sqs_missing_send_message_id_is_an_error() {
    run_rows(&[(
        "a send without a message ID",
        missing_message_id_is_an_unknown_outcome,
    )]);
}

#[tokio::test(flavor = "current_thread")]
async fn sqs_connect_on_a_current_thread_runtime_is_no_runtime() {
    let peer = SqsPeer::start();
    let outcome = peer.builder().connect().await;
    let accepted = accepted_after_finish(peer);

    assert!(
        matches!(outcome, Err(RuntimeError::NoRuntime)),
        "a bare current-thread runtime is not a Camber runtime: {:?}",
        outcome.map(drop)
    );
    assert_eq!(accepted, 0, "a refused connect performs no I/O");
}

#[test]
fn sqs_connect_without_any_runtime_is_no_runtime() {
    let peer = SqsPeer::start();
    let mut connect = pin!(peer.builder().connect());
    let polled = connect
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
    let accepted = accepted_after_finish(peer);

    match polled {
        Poll::Ready(Err(RuntimeError::NoRuntime)) => {}
        Poll::Ready(other) => panic!("connect without a runtime answered {:?}", other.map(drop)),
        Poll::Pending => panic!("connect without a runtime waited instead of refusing"),
    }
    assert_eq!(accepted, 0, "a refused connect performs no I/O");
}
