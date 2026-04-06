use super::{block_on, mq_error};
use crate::RuntimeError;
use crate::runtime;
use bytes::Bytes;
use std::time::Duration;

/// A received NATS message.
pub struct Message {
    inner: async_nats::Message,
}

impl Message {
    /// The message payload as bytes.
    pub fn payload(&self) -> &[u8] {
        &self.inner.payload
    }

    /// The subject this message was published to.
    pub fn subject(&self) -> &str {
        self.inner.subject.as_str()
    }
}

/// A NATS subscription that receives messages synchronously.
///
/// Wraps async-nats `Subscriber` with blocking receive operations
/// suitable for use in sync handlers.
pub struct Subscription {
    inner: async_nats::Subscriber,
}

impl Subscription {
    /// Block until the next message arrives or the timeout elapses.
    pub fn next_timeout(&mut self, timeout: Duration) -> Result<Message, RuntimeError> {
        runtime::check_cancel()?;
        let msg = block_on(async {
            tokio::time::timeout(timeout, {
                use futures_util::StreamExt;
                self.inner.next()
            })
            .await
        });
        runtime::check_cancel()?;
        match msg {
            Ok(Some(m)) => Ok(Message { inner: m }),
            Ok(None) => Err(RuntimeError::ChannelClosed),
            Err(_) => Err(RuntimeError::Timeout),
        }
    }

    /// Try to receive a message without blocking. Returns `None` if no message is ready.
    pub fn try_next(&mut self) -> Option<Message> {
        use futures_util::StreamExt;
        block_on(async {
            match tokio::time::timeout(Duration::from_millis(1), self.inner.next()).await {
                Ok(Some(m)) => Some(Message { inner: m }),
                _ => None,
            }
        })
    }
}

/// A sync NATS connection for use in sync handlers.
///
/// All operations use `block_in_place` internally, so they are safe to call
/// from Camber sync handlers without blocking the Tokio runtime.
#[derive(Clone)]
pub struct Connection {
    client: async_nats::Client,
}

/// Connect to a NATS server. Blocks until connected.
///
/// Suitable for sync handlers. For async handlers, use [`connect_async`].
pub fn connect(url: &str) -> Result<Connection, RuntimeError> {
    runtime::check_cancel()?;
    let client = block_on(async_nats::connect(url)).map_err(mq_error)?;
    Ok(Connection { client })
}

/// Connect to a NATS server asynchronously. For use in async handlers.
///
/// Returns the same `Connection` type — usable from both sync and async contexts.
/// The underlying client is inherently async; sync methods use `block_in_place`.
pub async fn connect_async(url: &str) -> Result<Connection, RuntimeError> {
    let client = async_nats::connect(url).await.map_err(mq_error)?;
    Ok(Connection { client })
}

impl Connection {
    /// Publish a message to the given subject.
    pub fn publish(&self, subject: &str, payload: &[u8]) -> Result<(), RuntimeError> {
        runtime::check_cancel()?;
        block_on(
            self.client
                .publish(subject.to_owned(), Bytes::copy_from_slice(payload)),
        )
        .map_err(mq_error)?;
        block_on(self.client.flush()).map_err(mq_error)?;
        runtime::check_cancel()?;
        Ok(())
    }

    /// Subscribe to a subject. Returns a [`Subscription`] that delivers messages synchronously.
    pub fn subscribe(&self, subject: &str) -> Result<Subscription, RuntimeError> {
        runtime::check_cancel()?;
        let sub = block_on(self.client.subscribe(subject.to_owned())).map_err(mq_error)?;
        Ok(Subscription { inner: sub })
    }

    /// Subscribe to a subject with a queue group.
    ///
    /// Messages are distributed across subscribers in the same queue group —
    /// each message is delivered to exactly one member.
    pub fn queue_subscribe(
        &self,
        subject: &str,
        queue_group: &str,
    ) -> Result<Subscription, RuntimeError> {
        runtime::check_cancel()?;
        let sub = block_on(
            self.client
                .queue_subscribe(subject.to_owned(), queue_group.to_owned()),
        )
        .map_err(mq_error)?;
        Ok(Subscription { inner: sub })
    }

    /// Publish a message asynchronously. For use in async handlers.
    pub async fn publish_async(&self, subject: &str, payload: &[u8]) -> Result<(), RuntimeError> {
        self.client
            .publish(subject.to_owned(), Bytes::copy_from_slice(payload))
            .await
            .map_err(mq_error)?;
        self.client.flush().await.map_err(mq_error)?;
        Ok(())
    }

    /// Subscribe asynchronously. For use in async handlers.
    ///
    /// Returns the same [`Subscription`] type — you can still use `next_timeout`
    /// from an async context (it uses `block_in_place` internally).
    pub async fn subscribe_async(&self, subject: &str) -> Result<Subscription, RuntimeError> {
        let sub = self
            .client
            .subscribe(subject.to_owned())
            .await
            .map_err(mq_error)?;
        Ok(Subscription { inner: sub })
    }
}
