//! One received SQS message.

/// A received SQS message.
#[derive(Debug)]
pub struct Message {
    body: Option<Box<str>>,
    receipt_handle: Option<Box<str>>,
    message_id: Option<Box<str>>,
}

impl Message {
    /// Take the fields of one SDK message, without copying them.
    pub(super) fn from_sdk(message: aws_sdk_sqs::types::Message) -> Self {
        Self {
            body: message.body.map(String::into_boxed_str),
            receipt_handle: message.receipt_handle.map(String::into_boxed_str),
            message_id: message.message_id.map(String::into_boxed_str),
        }
    }

    /// The message body, if present.
    #[must_use]
    pub fn body(&self) -> Option<&str> {
        self.body.as_deref()
    }

    /// The receipt handle that deletes this delivery of the message.
    #[must_use]
    pub fn receipt_handle(&self) -> Option<&str> {
        self.receipt_handle.as_deref()
    }

    /// The SQS message ID.
    #[must_use]
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }
}
