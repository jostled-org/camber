//! One NATS subscription and the messages it delivers.

use super::failure::{close_timed_out, failure};
use crate::integration_lifecycle::{
    IntegrationEntryObserver, IntegrationEntryState, TerminalOperation, integration,
};
use crate::mq::limits::admit_message;
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
    RuntimeError,
};
use futures_util::{FutureExt, StreamExt};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

/// A received NATS message.
pub struct Message {
    inner: async_nats::Message,
}

impl Message {
    /// The message payload as bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.inner.payload
    }

    /// The subject this message was published to.
    #[must_use]
    pub fn subject(&self) -> &str {
        self.inner.subject.as_str()
    }
}

/// One subscription of a [`Connection`](super::Connection).
///
/// It completes when it is closed, when its connection closes, or when the
/// SDK ends it. Messages the SDK already buffered are still delivered first.
pub struct Subscription {
    inner: async_nats::Subscriber,
    /// The connection's subscription slot, until close returns it.
    permit: Option<OwnedSemaphorePermit>,
    connection: IntegrationEntryObserver,
    instance: u64,
    max_message_bytes: usize,
    shutdown_timeout: Duration,
    /// The one close result, once closed.
    closed: Option<Result<(), Arc<IntegrationError>>>,
}

impl Subscription {
    pub(super) fn new(
        inner: async_nats::Subscriber,
        permit: OwnedSemaphorePermit,
        connection: IntegrationEntryObserver,
        instance: u64,
        max_message_bytes: usize,
        shutdown_timeout: Duration,
    ) -> Self {
        Self {
            inner,
            permit: Some(permit),
            connection,
            instance,
            max_message_bytes,
            shutdown_timeout,
            closed: None,
        }
    }

    /// Wait for the next message.
    ///
    /// `None` means the subscription completed; it never means a timeout.
    /// Every call is one receive terminal, a completed stream included.
    ///
    /// # Errors
    ///
    /// `LimitExceeded` for a message over the payload maximum, which is
    /// dropped undelivered. The subscription stays open.
    pub async fn next(&mut self) -> Result<Option<Message>, RuntimeError> {
        let owed = self.terminal(IntegrationOperation::Receive).admitted();
        let received = self.receive().await;
        owed.settled(self.deliver(received))
    }

    /// Wait at most `bound` for the next message.
    ///
    /// # Errors
    ///
    /// `Timeout` when the bound passes first, which receives nothing, and
    /// every error of [`Self::next`].
    pub async fn next_timeout(&mut self, bound: Duration) -> Result<Option<Message>, RuntimeError> {
        let owed = self.terminal(IntegrationOperation::Receive).admitted();
        let received = match tokio::time::timeout(bound, self.receive()).await {
            Ok(received) => self.deliver(received),
            Err(_) => Err(self.error(IntegrationFailure::Timeout, Retryability::Safe)),
        };
        owed.settled(received)
    }

    /// Take a message that is already buffered, without waiting.
    ///
    /// `None` means nothing is buffered now. A poll of a completed
    /// subscription is refused before admission.
    ///
    /// # Errors
    ///
    /// `Closed` once the subscription completed, and `LimitExceeded` for a
    /// message over the payload maximum.
    pub fn try_next(&mut self) -> Result<Option<Message>, RuntimeError> {
        let receive = self.terminal(IntegrationOperation::Receive);
        match tokio::task::unconstrained(self.inner.next()).now_or_never() {
            Some(Some(message)) => receive.admitted().settled(self.deliver(Some(message))),
            Some(None) => receive.refusing(Err(self.completed_error())),
            None if self.completed() => receive.refusing(Err(self.completed_error())),
            None => receive.admitted().settled(Ok(None)),
        }
    }

    /// Unsubscribe and resolve with the one fixed close result.
    ///
    /// Idempotent. Messages already buffered stay readable until the
    /// subscription completes. The first close is one close terminal; a
    /// repeated read reports nothing.
    ///
    /// # Errors
    ///
    /// `Timeout` when the SDK could not queue the unsubscribe within the
    /// connection's shutdown bound.
    pub async fn close(&mut self) -> Result<(), RuntimeError> {
        let closed = match self.closed.as_ref() {
            Some(closed) => closed.clone(),
            None => {
                let owed = self.terminal(IntegrationOperation::Close).admitted();
                let closed = self.unsubscribe().await;
                self.closed = Some(closed.clone());
                return owed.settled(closed.map_err(RuntimeError::Integration));
            }
        };
        closed.map_err(RuntimeError::Integration)
    }

    /// Take the SDK's next message, or `None` once the stream or the
    /// connection ended.
    async fn receive(&mut self) -> Option<async_nats::Message> {
        let settled = self.connection.settled();
        tokio::select! {
            biased;
            received = self.inner.next() => received,
            // A connection that settled cannot deliver again, even when the
            // SDK never ended the stream.
            () = settled => None,
        }
    }

    /// `operation` of this subscription's connection, as its terminal names
    /// it.
    const fn terminal(&self, operation: IntegrationOperation) -> TerminalOperation {
        TerminalOperation::new(IntegrationKind::Nats, operation, Some(self.instance))
    }

    async fn unsubscribe(&mut self) -> Result<(), Arc<IntegrationError>> {
        let permit = self.permit.take();
        let unsubscribed =
            tokio::time::timeout(self.shutdown_timeout, self.inner.unsubscribe()).await;
        drop(permit);
        match unsubscribed {
            // A refused unsubscribe means the SDK connection already ended,
            // which ended this subscription with it.
            Ok(Ok(()) | Err(_)) => Ok(()),
            Err(_) => Err(Arc::new(close_timed_out().with_instance(self.instance))),
        }
    }

    /// Whether this subscription or its connection has ended.
    fn completed(&self) -> bool {
        self.closed.is_some() || self.connection.state() == IntegrationEntryState::Settled
    }

    /// Hand `received` to the application, or refuse an oversized payload.
    fn deliver(
        &self,
        received: Option<async_nats::Message>,
    ) -> Result<Option<Message>, RuntimeError> {
        match received {
            Some(message) => admit_message(
                IntegrationKind::Nats,
                IntegrationOperation::Receive,
                message.payload.len(),
                self.max_message_bytes,
            )
            .map(|()| Some(Message { inner: message }))
            .map_err(|refused| integration(refused.with_instance(self.instance))),
            None => Ok(None),
        }
    }

    /// The refusal a completed subscription answers a poll with.
    fn completed_error(&self) -> RuntimeError {
        self.error(IntegrationFailure::Closed, Retryability::Never)
    }

    fn error(&self, kind: IntegrationFailure, retryability: Retryability) -> RuntimeError {
        integration(
            failure(IntegrationOperation::Receive, kind, retryability).with_instance(self.instance),
        )
    }
}
