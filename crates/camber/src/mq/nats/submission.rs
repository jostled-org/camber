//! The work of one admitted publish or subscribe, from the SDK's queue to the
//! answer that settles it, under one deadline.
//!
//! The submission mark is set the moment the SDK holds the command: from then
//! on a dropped waiter cannot take it back. A Core publish settles on the
//! SDK's flush; an acknowledged publish settles on its correlated receipt.

use super::ack::Registration;
use super::failure::{
    connection_replaced, deadline_passed, flush_lost, publish_refused, subscribe_refused,
};
use crate::IntegrationError;
use crate::IntegrationOperation;
use crate::integration_lifecycle::SubmissionMark;
use crate::runtime_test_support::{PublishCheckpoint, QueueCheckpoint};
use async_nats::client::PublishError;
use async_nats::{Client, Subject};
use bytes::Bytes;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{Instant, timeout_at};

/// One server connection of the SDK, by the count of connections it has
/// established.
///
/// A reconnect replaces the SDK's connection and discards its unwritten
/// commands, then flushes the new connection. A flush or a reply on another
/// transport proves nothing about a command submitted on this one.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Transport(u64);

impl Transport {
    pub(super) fn current(client: &Client) -> Self {
        Self(client.statistics().connects.load(Ordering::Acquire))
    }
}

/// What one admitted command's work needs from its connection.
pub(super) struct Submission {
    pub(super) client: Client,
    pub(super) operation: IntegrationOperation,
    pub(super) transport: Transport,
    pub(super) bound: Duration,
    pub(super) publish_checkpoint: Option<Arc<PublishCheckpoint>>,
    pub(super) queue_checkpoint: Option<Arc<QueueCheckpoint>>,
}

/// What an acknowledged publish owns beyond its submission: its correlation
/// entry, its payload, and the disconnects seen since before its admission.
pub(super) struct AcknowledgedPublish {
    pub(super) registration: Registration,
    pub(super) payload: Bytes,
    pub(super) disconnects: watch::Receiver<()>,
}

/// Queue one publish and wait for its flush, under one deadline.
pub(super) async fn publish_flushed(
    submission: Submission,
    subject: Subject,
    payload: Bytes,
    mark: SubmissionMark,
) -> Result<(), IntegrationError> {
    let deadline = Instant::now() + submission.bound;
    let queued = submission.client.publish(subject, payload);
    submitted(&submission, deadline, queued, &mark).await?;
    flushed(&submission, deadline).await
}

/// Queue one publish naming its private reply, and wait for its correlated
/// receipt, under one deadline.
///
/// `prepared` is the publish's registration and payload, or the refusal
/// registration answered before anything was sent.
pub(super) async fn publish_acknowledged(
    submission: Submission,
    subject: Subject,
    prepared: Result<AcknowledgedPublish, IntegrationError>,
    mark: SubmissionMark,
) -> Result<(), IntegrationError> {
    let AcknowledgedPublish {
        mut registration,
        payload,
        mut disconnects,
    } = prepared?;
    let deadline = Instant::now() + submission.bound;
    let queued = submission.client.publish_with_reply_and_headers(
        subject,
        registration.reply_subject(),
        registration.headers(),
        payload,
    );
    submitted(&submission, deadline, queued, &mark).await?;
    let reply = timeout_at(deadline, async {
        tokio::select! {
            biased;
            reply = registration.reply() => reply,
            // A lost transport may have dropped the command or its receipt.
            _ = disconnects.changed() => Err(connection_replaced(submission.operation)),
        }
    })
    .await
    .map_err(|_| deadline_passed(submission.operation, true))??;
    on_admitted_transport(&submission)?;
    registration.accept(&reply)
}

/// Queue one subscription and wait for its flush, under one deadline.
pub(super) async fn subscribe_flushed(
    submission: Submission,
    subject: Subject,
    queue_group: Option<Box<str>>,
) -> Result<async_nats::Subscriber, IntegrationError> {
    let deadline = Instant::now() + submission.bound;
    let client = &submission.client;
    let queued = match queue_group {
        Some(group) => {
            timeout_at(
                deadline,
                client.queue_subscribe(subject, String::from(group)),
            )
            .await
        }
        None => timeout_at(deadline, client.subscribe(subject)).await,
    };
    let subscriber = queued
        .map_err(|_| deadline_passed(submission.operation, false))?
        .map_err(subscribe_refused)?;
    flushed(&submission, deadline).await?;
    Ok(subscriber)
}

/// Hand the SDK one `queued` publish before `deadline`, set `mark` once it
/// holds the command, and pass any test hold after that mark.
async fn submitted(
    submission: &Submission,
    deadline: Instant,
    queued: impl Future<Output = Result<(), PublishError>>,
    mark: &SubmissionMark,
) -> Result<(), IntegrationError> {
    timeout_at(deadline, async {
        if let Some(checkpoint) = &submission.publish_checkpoint {
            checkpoint.wait().await;
        }
        match &submission.queue_checkpoint {
            Some(checkpoint) => checkpoint.observe(queued).await,
            None => queued.await,
        }
    })
    .await
    .map_err(|_| deadline_passed(submission.operation, false))?
    .map_err(publish_refused)?;
    mark.submit();
    if let Some(checkpoint) = &submission.queue_checkpoint {
        timeout_at(deadline, checkpoint.before_flush())
            .await
            .map_err(|_| deadline_passed(submission.operation, true))?;
    }
    Ok(())
}

/// Wait for the SDK to flush a submitted command on the transport it was
/// admitted on, before `deadline`.
async fn flushed(submission: &Submission, deadline: Instant) -> Result<(), IntegrationError> {
    let operation = submission.operation;
    timeout_at(deadline, submission.client.flush())
        .await
        .map_err(|_| deadline_passed(operation, true))?
        .map_err(|error| flush_lost(operation, error))?;
    on_admitted_transport(submission)
}

/// Refuse an answer read on another transport than the command was admitted
/// on.
fn on_admitted_transport(submission: &Submission) -> Result<(), IntegrationError> {
    match Transport::current(&submission.client) == submission.transport {
        true => Ok(()),
        false => Err(connection_replaced(submission.operation)),
    }
}
