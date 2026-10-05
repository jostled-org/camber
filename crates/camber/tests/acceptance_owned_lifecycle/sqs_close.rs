//! A forced stop fixes the SQS close failure for every access clone.
#![cfg(feature = "sqs")]

use crate::integration_rows::{
    ROW_BOUND, Row, all, expect, expect_eq, expect_polled_pending, integration_aggregate, refusal,
    timed_out,
};
use crate::scripted_peer::lock;
use crate::sqs_peer::SqsPeer;
use aws_sdk_sqs::config::{Credentials, ProvideCredentials};
use camber::mq::sqs::Client;
use camber::runtime_test_support::SqsCredentialProbe;
use camber::{IntegrationKind, IntegrationOperation, RuntimeError, runtime};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

#[test]
fn sqs_forced_stop_fixes_close_failure_for_every_clone() {
    let peer = SqsPeer::start();
    let (provider, hold) = held_credentials();
    let builder = SqsCredentialProbe::with_provider(peer.builder(), provider);
    let queue = peer.queue_url("forced-close");
    let mut waiter_slot = None;
    let mut client_slot = None;
    let outcome = runtime::builder()
        .shutdown_timeout(Duration::from_millis(100))
        .run(|| -> Row {
            let client = runtime::block_on(builder.connect())
                .map_err(|error| format!("connect: {error:?}"))?;
            client_slot = Some(client.clone());
            let mut waiter = Box::pin(async move { client.send_message(&queue, "held").await });
            runtime::block_on(async {
                expect_polled_pending("the send", &futures_util::poll!(waiter.as_mut()))
            })?;
            waiter_slot = Some(waiter);
            hold.entered
                .recv_timeout(ROW_BOUND)
                .map_err(|_| "the credential source never blocked its worker".to_owned())
        });
    let claims = match client_slot.as_ref() {
        Some(client) => fixed_close_claims(client, &outcome),
        None => Err(format!("no client escaped: {outcome:?}")),
    };
    drop(client_slot);
    drop(waiter_slot);
    let cleanup = hold.finish();
    let no_io = expect_eq(
        "requests before credentials",
        peer.control().log().accepted,
        0,
    );
    let checked = all([claims, cleanup, no_io, peer.finish(ROW_BOUND)]);
    assert!(checked.is_ok(), "{checked:?}");
}

fn fixed_close_claims(client: &Client, outcome: &Result<Row, RuntimeError>) -> Row {
    let clone = client.clone();
    let results = crate::common::block_on_detached(async {
        tokio::time::timeout(ROW_BOUND, async {
            let (first, second) = tokio::join!(client.close(), clone.close());
            [first, second, client.close().await]
        })
        .await
    })
    .map_err(|_| "close failed to expose its fixed settlement".to_owned())?;
    let expected = timed_out(IntegrationOperation::Close);
    let mut claims: Vec<Row> = results
        .iter()
        .map(|result| {
            expect_eq(
                "close after forced stop",
                result.as_ref().err().and_then(refusal),
                Some(expected),
            )
        })
        .collect();
    let same = match &results {
        [
            Err(RuntimeError::Integration(first)),
            Err(RuntimeError::Integration(second)),
            Err(RuntimeError::Integration(later)),
        ] => Arc::ptr_eq(first, second) && Arc::ptr_eq(first, later),
        _ => false,
    };
    claims.push(expect("the closers read different failures", same));
    let id = results[0].as_ref().err().and_then(|error| match error {
        RuntimeError::Integration(error) => error.instance_id(),
        _ => None,
    });
    claims.push(match outcome {
        Err(error) => integration_aggregate(error, IntegrationKind::Sqs).and_then(|entries| {
            expect_eq(
                "one retained close account",
                entries
                    .iter()
                    .map(|(id, value)| (Some(*id), *value))
                    .collect::<Vec<_>>(),
                vec![(id, expected)],
            )
        }),
        other => Err(format!("the forced stop returned no aggregate: {other:?}")),
    });
    all(claims)
}

/// The test owns release; a failed assertion cannot leave the SDK worker parked.
struct CredentialHold {
    entered: mpsc::Receiver<()>,
    release: Option<mpsc::Sender<()>>,
    dropped: mpsc::Receiver<()>,
}

impl CredentialHold {
    fn finish(mut self) -> Row {
        drop(self.release.take());
        self.dropped
            .recv_timeout(ROW_BOUND)
            .map_err(|_| "the SDK retained its blocked credential source".to_owned())
    }
}

impl Drop for CredentialHold {
    fn drop(&mut self) {
        drop(self.release.take());
    }
}

#[derive(Debug)]
struct HeldCredentials {
    loads: AtomicUsize,
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    dropped: mpsc::Sender<()>,
}

fn held_credentials() -> (HeldCredentials, CredentialHold) {
    let (entering, entered) = mpsc::sync_channel(1);
    let (release, released) = mpsc::channel();
    let (dropping, dropped) = mpsc::channel();
    (
        HeldCredentials {
            loads: AtomicUsize::new(0),
            entered: entering,
            release: Mutex::new(released),
            dropped: dropping,
        },
        CredentialHold {
            entered,
            release: Some(release),
            dropped,
        },
    )
}

impl ProvideCredentials for HeldCredentials {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::new(async move {
            if self.loads.fetch_add(1, Ordering::AcqRel) > 0 {
                // A test that stopped waiting is not a fault, and a dropped
                // release sender is the release itself.
                let _entered = self.entered.send(());
                let _released = lock(&self.release).recv();
                return Err(
                    aws_credential_types::provider::error::CredentialsError::not_loaded_no_source(),
                );
            }
            Ok(Credentials::new(
                "camber-held",
                "camber-held-secret",
                None,
                None,
                "test",
            ))
        })
    }
}

impl Drop for HeldCredentials {
    fn drop(&mut self) {
        let _reported = self.dropped.send(());
    }
}
