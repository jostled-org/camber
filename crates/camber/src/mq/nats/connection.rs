//! One admitted NATS connection and the operations it runs.

use super::ack::{Acknowledgements, ReplyReceiver};
use super::builder::NatsSettings;
use super::events::EventConsumer;
use super::failure::{close_timed_out, connect_failed, failure, readiness_lost};
use super::submission::{
    AcknowledgedPublish, Submission, Transport, publish_acknowledged, publish_flushed,
    subscribe_flushed,
};
use super::subscription::Subscription;
use crate::integration_lifecycle::{
    CloseOwner, IntegrationAccess, IntegrationEntryState, SubmissionMark, busy,
};
use crate::mq::connect::within_connect_bound;
use crate::mq::limits::admit_message;
use crate::runtime_test_support::{PublishCheckpoint, QueueCheckpoint};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
    RuntimeError,
};
use async_nats::connection::State;
use async_nats::{Client, Event, Subject};
use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;

/// A connection to a NATS server, admitted to the runtime that connected it.
///
/// Cheap to clone; every clone is the same connection. Dropping the last clone
/// requests close; the runtime still owns the connection until its close
/// settles.
#[derive(Clone)]
pub struct Connection {
    shared: Arc<Shared>,
}

/// What every clone of one connection shares.
struct Shared {
    access: IntegrationAccess,
    client: Client,
    events: Arc<EventConsumer>,
    operation_timeout: Duration,
    shutdown_timeout: Duration,
    max_message_bytes: usize,
    subscriptions: Arc<Semaphore>,
    /// The acknowledgement state, when publishing is acknowledged.
    acknowledgements: Option<Arc<Acknowledgements>>,
    publish_checkpoint: Option<Arc<PublishCheckpoint>>,
    queue_checkpoint: Option<Arc<QueueCheckpoint>>,
}

/// Connect an admitted instance and hand it to its close owner.
///
/// A failure drops the access, which settles the instance before the caller
/// reads the error. Readiness arms the connection's close terminal, so a
/// connection that never opened reports no close.
pub(super) async fn establish(
    access: IntegrationAccess,
    settings: NatsSettings,
) -> Result<Connection, RuntimeError> {
    let events = Arc::new(EventConsumer::new(access.monitor()));
    let (client, replies) = within_connect_bound(
        &access,
        IntegrationKind::Nats,
        settings.limits.connect_timeout,
        handshake(&settings, Arc::clone(&events)),
    )
    .await?;
    access.connected()?;
    let acknowledgements = replies.as_ref().map(ReplyReceiver::acknowledgements);
    let closed = events.closed();
    let local = settings.limits.shutdown_timeout;
    let owned = client.clone();
    access.spawn_close_owner(move |owner| own_close(owner, owned, closed, local, replies))?;
    Ok(Connection {
        shared: Arc::new(Shared {
            access,
            client,
            events,
            operation_timeout: settings.limits.operation_timeout,
            shutdown_timeout: settings.limits.shutdown_timeout,
            max_message_bytes: settings.limits.max_message_bytes,
            subscriptions: Arc::new(Semaphore::new(settings.max_subscriptions)),
            acknowledgements,
            publish_checkpoint: None,
            queue_checkpoint: None,
        }),
    })
}

/// Connect, install the private reply subscription when publishing is
/// acknowledged, and flush both.
async fn handshake(
    settings: &NatsSettings,
    events: Arc<EventConsumer>,
) -> Result<(Client, Option<ReplyReceiver>), IntegrationError> {
    let options = async_nats::ConnectOptions::new()
        .client_capacity(settings.client_capacity)
        .subscription_capacity(settings.subscription_capacity)
        .connection_timeout(settings.limits.connect_timeout)
        .event_callback(move |event| {
            let events = Arc::clone(&events);
            async move { events.consume(event) }
        });
    // The SDK copies a borrowed list once into its own server pool.
    let client = options
        .connect(&*settings.servers)
        .await
        .map_err(connect_failed)?;
    let replies = match &settings.acknowledged_stream {
        Some(stream) => Some(ReplyReceiver::install(&client, stream.clone()).await?),
        None => None,
    };
    client.flush().await.map_err(readiness_lost)?;
    Ok((client, replies))
}

/// Close the transport once the entry is closing with no work running.
///
/// An acknowledged connection's `replies` route receipts while admitted
/// publishes drain, and end before the transport closes, so the owner
/// finishes only after its receiver did. Drain only queues the SDK's close;
/// the SDK's closed event is what proves it. A close the bound cuts short is
/// an incomplete close, never a success.
async fn own_close(
    mut owner: CloseOwner,
    client: Client,
    mut closed: watch::Receiver<bool>,
    local: Duration,
    replies: Option<ReplyReceiver>,
) {
    match replies {
        Some(replies) => replies.serve_until(owner.closing()).await,
        None => owner.closing().await,
    }
    let bound = owner.bound(local);
    let result = match timeout(bound, drain_until_closed(&client, &mut closed)).await {
        Ok(()) => Ok(()),
        Err(_) => Err(close_timed_out()),
    };
    drop(client);
    owner.finish(result);
}

/// Queue the drain and wait for the SDK to report its connection closed.
async fn drain_until_closed(client: &Client, closed: &mut watch::Receiver<bool>) {
    // A refused drain means the SDK's connection task already ended: there
    // is nothing left to close.
    if client.drain().await.is_err() {
        return;
    }
    // The sender lives as long as the SDK's event task; its end is closure too.
    drop(closed.wait_for(|closed| *closed).await);
}

impl Connection {
    pub(crate) fn hold_queue(&mut self, checkpoint: Arc<QueueCheckpoint>) -> Option<()> {
        Arc::get_mut(&mut self.shared)?.queue_checkpoint = Some(checkpoint);
        Some(())
    }

    pub(crate) fn hold_publish(&mut self, checkpoint: Arc<PublishCheckpoint>) -> Option<()> {
        Arc::get_mut(&mut self.shared)?.publish_checkpoint = Some(checkpoint);
        Some(())
    }

    /// Whether the connection admits work and the SDK is connected now.
    ///
    /// A local observation, not a promise that the next publish succeeds.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed, and `Unavailable` while the SDK is
    /// disconnected or reconnecting.
    pub fn ready(&self) -> Result<(), RuntimeError> {
        let shared = &self.shared;
        let ready = IntegrationOperation::Ready;
        shared.access.checked(ready, shared.admitting(ready))?;
        shared.access.terminal(ready).admitted().settled(Ok(()))
    }

    /// Publish `payload` to `subject`.
    ///
    /// Core publishing succeeds once the SDK flushed the message to its
    /// socket. With [`NatsBuilder::acknowledged_publishing`] it succeeds only
    /// once the server's correlated acknowledgement names the configured
    /// stream.
    ///
    /// # Errors
    ///
    /// Refusals that sent nothing: `LimitExceeded` for a payload over the
    /// maximum, `Rejected` for an invalid subject, `Unavailable` while
    /// disconnected, `Closed` after close, and `Busy` at the operation limit
    /// or a full report budget. After submission: `Timeout` or
    /// `OutcomeUnknown` with unknown retryability, because the server may
    /// have the message. An acknowledged publish also reads its receipt's
    /// refusal: `Unavailable` with no responders, `Rejected` for a stream
    /// mismatch or another JetStream error, `LimitExceeded` for the server's
    /// size limits, and `PermissionDenied`; and `OutcomeUnknown` for a
    /// receipt it cannot read as either.
    ///
    /// [`NatsBuilder::acknowledged_publishing`]: super::NatsBuilder::acknowledged_publishing
    pub async fn publish(&self, subject: &str, payload: &[u8]) -> Result<(), RuntimeError> {
        let shared = &self.shared;
        // Watched before admission reads the connection state, so a
        // disconnect after that read is always seen.
        let acknowledged = shared
            .acknowledgements
            .as_ref()
            .map(|acknowledgements| (Arc::clone(acknowledgements), shared.events.disconnects()));
        let transport = shared.access.checked(
            IntegrationOperation::Publish,
            admit_message(
                IntegrationKind::Nats,
                IntegrationOperation::Publish,
                payload.len(),
                shared.max_message_bytes,
            )
            .and_then(|()| shared.admitting(IntegrationOperation::Publish)),
        )?;
        let admitted = shared.access.admit(IntegrationOperation::Publish)?;
        let mark = SubmissionMark::default();
        let submission = shared.submission(IntegrationOperation::Publish, transport);
        let subject = Subject::from(subject);
        let waiter = match acknowledged {
            None => {
                let payload = Bytes::copy_from_slice(payload);
                admitted.submit_marked(
                    publish_flushed(submission, subject, payload, mark.clone()),
                    mark,
                )
            }
            Some((acknowledgements, disconnects)) => {
                let prepared = prepare_acknowledged(&acknowledgements, payload, disconnects);
                admitted.submit_marked(
                    publish_acknowledged(submission, subject, prepared, mark.clone()),
                    mark,
                )
            }
        };
        waiter?.wait().await
    }

    /// Subscribe to `subject`.
    ///
    /// # Errors
    ///
    /// See [`Self::queue_subscribe`].
    pub async fn subscribe(&self, subject: &str) -> Result<Subscription, RuntimeError> {
        self.register(subject, None).await
    }

    /// Subscribe to `subject` as a member of `queue_group`: each message
    /// reaches one member of the group.
    ///
    /// # Errors
    ///
    /// Refusals that sent nothing: `Busy` at the subscription or operation
    /// limit or a full report budget, `Unavailable` while disconnected,
    /// `Closed` after close, and `Rejected` for an invalid subject or group.
    /// After submission: `Timeout` or `OutcomeUnknown`.
    pub async fn queue_subscribe(
        &self,
        subject: &str,
        queue_group: &str,
    ) -> Result<Subscription, RuntimeError> {
        self.register(subject, Some(queue_group)).await
    }

    /// Close the connection and every subscription, and resolve with the one
    /// fixed close result.
    ///
    /// Close commits before this resolves, and every clone refuses new work
    /// from then on. It waits for running operations, then for the SDK's
    /// closed event, within the shutdown bound.
    ///
    /// # Errors
    ///
    /// `Timeout` when the SDK never reported closure within the bound, and the
    /// failure that closed the connection, such as a delivered slow-consumer
    /// event.
    pub async fn close(&self) -> Result<(), RuntimeError> {
        self.shared.access.close().await
    }

    /// The acknowledgement state, when publishing is acknowledged.
    pub(crate) fn acknowledgements(&self) -> Option<Arc<Acknowledgements>> {
        self.shared.acknowledgements.clone()
    }

    /// Hand `event` to this connection's event consumer, as the SDK does.
    pub(crate) fn deliver_event(&self, event: Event) {
        self.shared.events.consume(event);
    }

    async fn register(
        &self,
        subject: &str,
        queue_group: Option<&str>,
    ) -> Result<Subscription, RuntimeError> {
        let shared = &self.shared;
        let subscribe = IntegrationOperation::Subscribe;
        let transport = shared
            .access
            .checked(subscribe, shared.admitting(subscribe))?;
        let permit = shared.access.checked(
            subscribe,
            Arc::clone(&shared.subscriptions)
                .try_acquire_owned()
                .map_err(|_| busy(IntegrationKind::Nats, subscribe)),
        )?;
        let admitted = shared.access.admit(IntegrationOperation::Subscribe)?;
        // Unmarked: a dropped waiter drops the SDK subscriber, which queues
        // its `UNSUB` behind the `SUB`, so the abandoned outcome is known.
        let work = subscribe_flushed(
            shared.submission(IntegrationOperation::Subscribe, transport),
            Subject::from(subject),
            queue_group.map(Box::from),
        );
        let subscriber = admitted.submit(work)?.wait().await?;
        Ok(Subscription::new(
            subscriber,
            permit,
            shared.access.observer(),
            shared.access.id(),
            shared.max_message_bytes,
            shared.shutdown_timeout,
        ))
    }
}

impl Shared {
    /// Refuse new work once close is committed or while disconnected.
    ///
    /// Admitted work gets the transport it was admitted on.
    fn admitting(&self, operation: IntegrationOperation) -> Result<Transport, IntegrationError> {
        let (kind, retryability) = match (self.access.state(), self.client.connection_state()) {
            (IntegrationEntryState::Closing | IntegrationEntryState::Settled, _) => {
                (IntegrationFailure::Closed, Retryability::Never)
            }
            (_, State::Connected) => return Ok(Transport::current(&self.client)),
            (_, State::Pending | State::Disconnected) => {
                (IntegrationFailure::Unavailable, Retryability::Safe)
            }
        };
        Err(failure(operation, kind, retryability))
    }

    /// What one admitted `operation` admitted on `transport` needs.
    fn submission(&self, operation: IntegrationOperation, transport: Transport) -> Submission {
        Submission {
            client: self.client.clone(),
            operation,
            transport,
            bound: self.operation_timeout,
            publish_checkpoint: self.publish_checkpoint.clone(),
            queue_checkpoint: self.queue_checkpoint.clone(),
        }
    }
}

/// Register one admitted acknowledged publish, then copy its payload.
///
/// Registration comes first, so a refusal it answers copies nothing.
fn prepare_acknowledged(
    acknowledgements: &Arc<Acknowledgements>,
    payload: &[u8],
    disconnects: watch::Receiver<()>,
) -> Result<AcknowledgedPublish, IntegrationError> {
    Ok(AcknowledgedPublish {
        registration: acknowledgements.register()?,
        payload: Bytes::copy_from_slice(payload),
        disconnects,
    })
}
