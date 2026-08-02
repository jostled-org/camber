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
    ))?;
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

fn wait_time_seconds(wait_time: Duration) -> Result<i32, RuntimeError> {
    const MAX_WAIT_TIME: Duration = Duration::from_secs(20);
    match wait_time <= MAX_WAIT_TIME {
        true => i32::try_from(wait_time.as_secs()).map_err(|error| {
            RuntimeError::MessageQueue(format!("invalid SQS wait time: {error}").into())
        }),
        false => Err(RuntimeError::MessageQueue(
            format!("wait_time must not exceed 20 seconds, got {wait_time:?}").into(),
        )),
    }
}

pub(crate) fn validate_receive_parameters(
    max_messages: i32,
    wait_time: Duration,
) -> Result<i32, RuntimeError> {
    validate_max_messages(max_messages)?;
    wait_time_seconds(wait_time)
}

pub(crate) fn required_message_id(message_id: Option<&str>) -> Result<Box<str>, RuntimeError> {
    match message_id {
        Some(message_id) if !message_id.is_empty() => Ok(message_id.into()),
        _ => Err(RuntimeError::MessageQueue(
            "SQS send response did not include a message ID".into(),
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
        )?
        .map_err(mq_error)?;
        runtime::check_cancel()?;
        required_message_id(result.message_id())
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
        let wait_time_seconds = validate_receive_parameters(max_messages, wait_time)?;
        runtime::check_cancel()?;
        let result = block_on(
            self.inner
                .receive_message()
                .queue_url(queue_url)
                .max_number_of_messages(max_messages)
                .wait_time_seconds(wait_time_seconds)
                .send(),
        )?
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
        )?
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
        required_message_id(result.message_id())
    }

    /// Receive messages asynchronously. For use in async handlers.
    pub async fn receive_messages_async(
        &self,
        queue_url: &str,
        max_messages: i32,
        wait_time: Duration,
    ) -> Result<Vec<Message>, RuntimeError> {
        let wait_time_seconds = validate_receive_parameters(max_messages, wait_time)?;
        let result = self
            .inner
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(max_messages)
            .wait_time_seconds(wait_time_seconds)
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
