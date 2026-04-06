use super::{block_on, mq_error};
use crate::RuntimeError;
use crate::runtime;
use std::time::Duration;

/// A received SQS message.
#[derive(Debug)]
pub struct Message {
    body: Option<Box<str>>,
    receipt_handle: Option<Box<str>>,
    message_id: Option<Box<str>>,
}

impl Message {
    /// The message body, if present.
    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    /// The receipt handle used to delete or change visibility of this message.
    pub fn receipt_handle(&self) -> Option<&str> {
        self.receipt_handle.as_deref()
    }

    /// The SQS message ID.
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }
}

/// A sync SQS client for use in sync handlers.
///
/// All operations use `block_in_place` internally.
#[derive(Clone)]
pub struct Client {
    inner: aws_sdk_sqs::Client,
}

/// Create an SQS client from the default AWS config.
///
/// Loads credentials and region from the environment (env vars, config files, IMDS).
/// Blocks until the config is loaded.
pub fn connect() -> Result<Client, RuntimeError> {
    runtime::check_cancel()?;
    let config = block_on(aws_config::load_defaults(
        aws_config::BehaviorVersion::latest(),
    ));
    let inner = aws_sdk_sqs::Client::new(&config);
    Ok(Client { inner })
}

/// Create an SQS client asynchronously.
pub async fn connect_async() -> Result<Client, RuntimeError> {
    let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let inner = aws_sdk_sqs::Client::new(&config);
    Ok(Client { inner })
}

fn map_sqs_message(m: &aws_sdk_sqs::types::Message) -> Message {
    Message {
        body: m.body().map(|b| b.into()),
        receipt_handle: m.receipt_handle().map(|r| r.into()),
        message_id: m.message_id().map(|i| i.into()),
    }
}

fn validate_max_messages(n: i32) -> Result<(), RuntimeError> {
    match (1..=10).contains(&n) {
        true => Ok(()),
        false => Err(RuntimeError::MessageQueue(
            format!("max_messages must be 1-10, got {n}").into(),
        )),
    }
}

impl Client {
    /// Send a message to an SQS queue.
    pub fn send_message(&self, queue_url: &str, body: &str) -> Result<Box<str>, RuntimeError> {
        runtime::check_cancel()?;
        let result = block_on(
            self.inner
                .send_message()
                .queue_url(queue_url)
                .message_body(body)
                .send(),
        )
        .map_err(mq_error)?;
        runtime::check_cancel()?;
        let id: Box<str> = result.message_id().unwrap_or("").into();
        Ok(id)
    }

    /// Receive messages from an SQS queue.
    ///
    /// `wait_time` enables long polling — the call blocks up to that duration
    /// waiting for messages before returning an empty list.
    pub fn receive_messages(
        &self,
        queue_url: &str,
        max_messages: i32,
        wait_time: Duration,
    ) -> Result<Vec<Message>, RuntimeError> {
        validate_max_messages(max_messages)?;
        runtime::check_cancel()?;
        let result = block_on(
            self.inner
                .receive_message()
                .queue_url(queue_url)
                .max_number_of_messages(max_messages)
                .wait_time_seconds(wait_time.as_secs() as i32)
                .send(),
        )
        .map_err(mq_error)?;
        runtime::check_cancel()?;
        Ok(result.messages().iter().map(map_sqs_message).collect())
    }

    /// Delete a message from an SQS queue using its receipt handle.
    pub fn delete_message(
        &self,
        queue_url: &str,
        receipt_handle: &str,
    ) -> Result<(), RuntimeError> {
        runtime::check_cancel()?;
        block_on(
            self.inner
                .delete_message()
                .queue_url(queue_url)
                .receipt_handle(receipt_handle)
                .send(),
        )
        .map_err(mq_error)?;
        runtime::check_cancel()?;
        Ok(())
    }

    /// Send a message asynchronously. For use in async handlers.
    pub async fn send_message_async(
        &self,
        queue_url: &str,
        body: &str,
    ) -> Result<Box<str>, RuntimeError> {
        let result = self
            .inner
            .send_message()
            .queue_url(queue_url)
            .message_body(body)
            .send()
            .await
            .map_err(mq_error)?;
        let id: Box<str> = result.message_id().unwrap_or("").into();
        Ok(id)
    }

    /// Receive messages asynchronously. For use in async handlers.
    pub async fn receive_messages_async(
        &self,
        queue_url: &str,
        max_messages: i32,
        wait_time: Duration,
    ) -> Result<Vec<Message>, RuntimeError> {
        validate_max_messages(max_messages)?;
        let result = self
            .inner
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(max_messages)
            .wait_time_seconds(wait_time.as_secs() as i32)
            .send()
            .await
            .map_err(mq_error)?;
        Ok(result.messages().iter().map(map_sqs_message).collect())
    }

    /// Delete a message asynchronously. For use in async handlers.
    pub async fn delete_message_async(
        &self,
        queue_url: &str,
        receipt_handle: &str,
    ) -> Result<(), RuntimeError> {
        self.inner
            .delete_message()
            .queue_url(queue_url)
            .receipt_handle(receipt_handle)
            .send()
            .await
            .map_err(mq_error)?;
        Ok(())
    }
}
