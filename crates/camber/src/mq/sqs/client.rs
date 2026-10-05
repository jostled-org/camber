//! One admitted SQS client and the operations it runs.

use super::builder::{CredentialSource, SqsSettings};
use super::failure::{
    batch_over_limit, credentials_failed, cut_by_close, deadline_passed, invalid_input, sdk_failed,
    unacknowledged_send,
};
use super::message::Message;
use super::submission::MarkSubmission;
use crate::error::Effect;
use crate::integration_lifecycle::{IntegrationAccess, IntegrationEntryObserver, SubmissionMark};
use crate::mq::connect::within_connect_bound;
use crate::mq::limits::{admit_message, invalid_setting};
use crate::{IntegrationError, IntegrationKind, IntegrationOperation, RuntimeError};
use aws_sdk_sqs::config::http::HttpResponse;
use aws_sdk_sqs::config::retry::RetryConfig;
use aws_sdk_sqs::config::timeout::TimeoutConfig;
use aws_sdk_sqs::config::{
    BehaviorVersion, Credentials, ProvideCredentials, Region, SharedCredentialsProvider,
    StalledStreamProtectionConfig,
};
use aws_sdk_sqs::error::{ProvideErrorMetadata, SdkError};
use std::error::Error;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

/// The most messages one receive asks for.
const MAX_BATCH: i32 = 10;

/// The longest receive wait the service allows.
const MAX_WAIT: Duration = Duration::from_secs(20);

/// An SQS client, admitted to the runtime that connected it.
///
/// Cheap to clone; every clone is the same client. Dropping the last clone
/// requests close; the runtime still owns the client until its close
/// settles.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

/// What every clone of one client shares.
struct Shared {
    access: IntegrationAccess,
    sdk: aws_sdk_sqs::Client,
    operation_timeout: Duration,
    shutdown_timeout: Duration,
    max_message_bytes: usize,
}

/// Load an admitted client's configuration and credentials under the
/// connect bound.
///
/// A failure drops the access, which settles the instance before the caller
/// reads the error. A loaded client arms its close terminal, so a client
/// that never loaded reports no close.
pub(super) async fn establish(
    access: IntegrationAccess,
    settings: SqsSettings,
) -> Result<Client, RuntimeError> {
    let limits = settings.limits;
    let sdk = within_connect_bound(
        &access,
        IntegrationKind::Sqs,
        limits.connect_timeout,
        configure(settings),
    )
    .await?;
    access.connected()?;
    Ok(Client {
        shared: Arc::new(Shared {
            access,
            sdk,
            operation_timeout: limits.operation_timeout,
            shutdown_timeout: limits.shutdown_timeout,
            max_message_bytes: limits.max_message_bytes,
        }),
    })
}

/// Build the SDK client with one attempt per request and Camber's deadline
/// as the only timeout, then load its credentials once.
///
/// With both region and credentials given, nothing ambient is read. The
/// settings move into the SDK's configuration rather than being copied.
async fn configure(settings: SqsSettings) -> Result<aws_sdk_sqs::Client, IntegrationError> {
    let region = settings
        .region
        .map(|region| Region::new(String::from(region)));
    let (builder, credentials) = match (region, provider(settings.credentials)) {
        (Some(region), Some(provider)) => (
            aws_sdk_sqs::Config::builder()
                .behavior_version(BehaviorVersion::latest())
                .region(region)
                .credentials_provider(provider.clone()),
            Some(provider),
        ),
        (region, provider) => ambient(region, provider).await,
    };
    let builder = builder
        .retry_config(RetryConfig::disabled())
        .timeout_config(TimeoutConfig::disabled())
        .stalled_stream_protection(StalledStreamProtectionConfig::disabled());
    let builder = match settings.endpoint {
        Some(endpoint) => builder.endpoint_url(endpoint),
        None => builder,
    };
    let config = builder.build();
    // No region or no credentials loaded: nothing was sent. The refusal names
    // which one is missing.
    let credentials = match (config.region(), credentials) {
        (None, _) => Err(invalid_setting(IntegrationKind::Sqs, "region")),
        (Some(_), None) => Err(invalid_setting(IntegrationKind::Sqs, "credentials")),
        (Some(_), Some(credentials)) => Ok(credentials),
    }?;
    credentials
        .provide_credentials()
        .await
        .map_err(credentials_failed)?;
    Ok(aws_sdk_sqs::Client::from_conf(config))
}

/// Load what the caller left out through the SDK's own chains, keeping what
/// the caller gave. Returns the configuration and the credential provider it
/// resolved, if any.
async fn ambient(
    region: Option<Region>,
    provider: Option<SharedCredentialsProvider>,
) -> (
    aws_sdk_sqs::config::Builder,
    Option<SharedCredentialsProvider>,
) {
    let loader = aws_config::defaults(BehaviorVersion::latest());
    let loader = match region {
        Some(region) => loader.region(region),
        None => loader,
    };
    let loader = match provider {
        Some(provider) => loader.credentials_provider(provider),
        None => loader,
    };
    let loaded = loader.load().await;
    (
        aws_sdk_sqs::config::Builder::from(&loaded),
        loaded.credentials_provider(),
    )
}

/// The provider an explicit source names, or `None` for the SDK's chain.
fn provider(source: CredentialSource) -> Option<SharedCredentialsProvider> {
    match source {
        CredentialSource::Chain => None,
        CredentialSource::Explicit {
            access_key,
            secret_key,
            session_token,
        } => Some(SharedCredentialsProvider::new(Credentials::new(
            access_key,
            secret_key,
            session_token.map(String::from),
            None,
            "camber",
        ))),
        CredentialSource::Provided(provider) => Some(provider),
    }
}

impl Client {
    /// Query `queue_url` once: the queue existed and answered this client
    /// at that instant.
    ///
    /// It proves nothing about send, receive, or delete permission.
    ///
    /// # Errors
    ///
    /// `Closed` after close and `Busy` at the operation limit or a full
    /// report budget, with nothing sent. From the service: `Rejected` for a
    /// missing queue and `PermissionDenied`, which repeating cannot fix; and
    /// `Unavailable` for a throttle or a lost answer, and `Timeout`, which are
    /// safe to repeat because the query is read-only.
    pub async fn ready(&self, queue_url: &str) -> Result<(), RuntimeError> {
        let shared = &self.shared;
        let admitted = shared.access.admit(IntegrationOperation::Ready)?;
        let attempt = shared.attempt(IntegrationOperation::Ready, Effect::ReadOnly);
        let request = shared
            .sdk
            .get_queue_attributes()
            .queue_url(queue_url)
            .customize()
            .interceptor(attempt.interceptor());
        let work = async move { attempt.run(request.send()).await.map(drop) };
        admitted.submit(work)?.wait().await
    }

    /// Send `body` to `queue_url` and return the message ID the service
    /// acknowledged it with.
    ///
    /// # Errors
    ///
    /// Refusals that sent nothing: `LimitExceeded` for a body over the
    /// maximum, `Closed` after close, and `Busy` at the operation limit or a
    /// full report budget. From the service: `Rejected` or
    /// `PermissionDenied`, and `Unavailable` for a throttle, which is safe to
    /// repeat. After submission: `Timeout`, `Cancelled`,
    /// `Unavailable`, or `OutcomeUnknown` with unknown retryability, because
    /// the queue may hold the message; an answer without a message ID is
    /// `OutcomeUnknown` too.
    pub async fn send_message(
        &self,
        queue_url: &str,
        body: &str,
    ) -> Result<Box<str>, RuntimeError> {
        let shared = &self.shared;
        shared.access.checked(
            IntegrationOperation::Publish,
            admit_message(
                IntegrationKind::Sqs,
                IntegrationOperation::Publish,
                body.len(),
                shared.max_message_bytes,
            ),
        )?;
        let admitted = shared.access.admit(IntegrationOperation::Publish)?;
        let attempt = shared.attempt(IntegrationOperation::Publish, Effect::SideEffect);
        let mark = attempt.mark.clone();
        let request = shared
            .sdk
            .send_message()
            .queue_url(queue_url)
            .message_body(body)
            .customize()
            .interceptor(attempt.interceptor());
        let work = async move {
            let output = attempt.run(request.send()).await?;
            acknowledged(output.message_id)
        };
        admitted.submit_marked(work, mark)?.wait().await
    }

    /// Receive up to `max_messages` messages from `queue_url`, long-polling
    /// up to `wait_time` for the first.
    ///
    /// Each receive is one request; Camber runs no polling loop. A receive
    /// hides its messages for the queue's visibility timeout, so it is not a
    /// harmless read.
    ///
    /// # Errors
    ///
    /// Refusals that sent nothing: `Rejected` for a batch size outside 1–10
    /// or a wait over 20 seconds, `Closed` after close, and `Busy` at the
    /// operation limit or a full report budget. A batch larger than asked for
    /// or holding a body over the maximum is `LimitExceeded`: no message
    /// reaches the caller, none is deleted, and each returns to the queue
    /// when its visibility ends. After submission a lost answer is
    /// `OutcomeUnknown`.
    pub async fn receive_messages(
        &self,
        queue_url: &str,
        max_messages: i32,
        wait_time: Duration,
    ) -> Result<Box<[Message]>, RuntimeError> {
        let shared = &self.shared;
        let wait_seconds = shared.access.checked(
            IntegrationOperation::Receive,
            receive_parameters(max_messages, wait_time),
        )?;
        let admitted = shared.access.admit(IntegrationOperation::Receive)?;
        let attempt = shared.attempt(IntegrationOperation::Receive, Effect::SideEffect);
        let mark = attempt.mark.clone();
        let request = shared
            .sdk
            .receive_message()
            .queue_url(queue_url)
            .max_number_of_messages(max_messages)
            .wait_time_seconds(wait_seconds)
            .customize()
            .interceptor(attempt.interceptor());
        let max_bytes = shared.max_message_bytes;
        let work = async move {
            let output = attempt.run(request.send()).await?;
            retained(output.messages.unwrap_or_default(), max_messages, max_bytes)
        };
        admitted.submit_marked(work, mark)?.wait().await
    }

    /// Delete the delivery `receipt_handle` names from `queue_url`.
    ///
    /// # Errors
    ///
    /// `Closed` after close and `Busy` at the operation limit or a full
    /// report budget, with nothing sent. From the service: `Rejected` for a
    /// stale or invalid receipt, `PermissionDenied`, and `Unavailable` for a
    /// throttle, which is safe to repeat. After submission a lost answer is
    /// `OutcomeUnknown`.
    pub async fn delete_message(
        &self,
        queue_url: &str,
        receipt_handle: &str,
    ) -> Result<(), RuntimeError> {
        let shared = &self.shared;
        let admitted = shared.access.admit(IntegrationOperation::Delete)?;
        let attempt = shared.attempt(IntegrationOperation::Delete, Effect::SideEffect);
        let mark = attempt.mark.clone();
        let request = shared
            .sdk
            .delete_message()
            .queue_url(queue_url)
            .receipt_handle(receipt_handle)
            .customize()
            .interceptor(attempt.interceptor());
        let work = async move { attempt.run(request.send()).await.map(drop) };
        admitted.submit_marked(work, mark)?.wait().await
    }

    /// Close the client and resolve with the one fixed close result.
    ///
    /// Close commits before this resolves, and every clone refuses new work
    /// from then on. Running operations may finish within `shutdown_timeout`;
    /// one still running then is cut and returns `Cancelled` to its own
    /// caller.
    ///
    /// # Errors
    ///
    /// `Timeout` with `Never` retryability if forced runtime shutdown could
    /// not settle an operation. The runtime retains that close failure;
    /// concurrent and later callers read the same fixed result.
    pub async fn close(&self) -> Result<(), RuntimeError> {
        self.shared.access.close().await
    }
}

impl Shared {
    /// The bounds and mark one operation's request runs under.
    fn attempt(&self, operation: IntegrationOperation, effect: Effect) -> Attempt {
        Attempt {
            operation,
            effect,
            mark: SubmissionMark::default(),
            bound: self.operation_timeout,
            observer: self.access.observer(),
            shutdown: self.shutdown_timeout,
        }
    }
}

/// One operation's request: its deadline, its close cut, and whether it
/// reached the transport.
struct Attempt {
    operation: IntegrationOperation,
    effect: Effect,
    mark: SubmissionMark,
    bound: Duration,
    observer: IntegrationEntryObserver,
    shutdown: Duration,
}

impl Attempt {
    fn interceptor(&self) -> MarkSubmission {
        MarkSubmission::new(self.mark.clone())
    }

    /// Run the request under the operation deadline and the close cut,
    /// classifying how it ended.
    async fn run<O, E>(
        self,
        sent: impl Future<Output = Result<O, SdkError<E, HttpResponse>>>,
    ) -> Result<O, IntegrationError>
    where
        E: Error + ProvideErrorMetadata + Send + Sync + 'static,
    {
        let deadline = Instant::now() + self.bound;
        let closing = async {
            self.observer.closing().await;
            tokio::time::sleep(self.shutdown).await;
        };
        let outcome = tokio::select! {
            biased;
            outcome = timeout_at(deadline, sent) => outcome,
            () = closing => {
                return Err(cut_by_close(self.operation, self.effect, self.mark.submitted()));
            }
        };
        let submitted = self.mark.submitted();
        match outcome {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => Err(sdk_failed(self.operation, self.effect, submitted, error)),
            Err(_) => Err(deadline_passed(self.operation, self.effect, submitted)),
        }
    }
}

/// Check a receive's batch size and wait, returning the wait in seconds.
fn receive_parameters(max_messages: i32, wait_time: Duration) -> Result<i32, IntegrationError> {
    let refused = || invalid_input(IntegrationOperation::Receive);
    match (
        (1..=MAX_BATCH).contains(&max_messages),
        wait_time <= MAX_WAIT,
    ) {
        (true, true) => i32::try_from(wait_time.as_secs()).map_err(|_| refused()),
        _ => Err(refused()),
    }
}

/// The message ID that names a send's acknowledgement.
fn acknowledged(message_id: Option<String>) -> Result<Box<str>, IntegrationError> {
    message_id
        .filter(|id| !id.is_empty())
        .map(String::into_boxed_str)
        .ok_or(unacknowledged_send())
}

/// Keep a batch only when every message fits the bounds; otherwise deliver
/// none of it.
fn retained(
    messages: Vec<aws_sdk_sqs::types::Message>,
    max_messages: i32,
    max_bytes: usize,
) -> Result<Box<[Message]>, IntegrationError> {
    let within_count = usize::try_from(max_messages).is_ok_and(|max| messages.len() <= max);
    let within_bytes = messages.iter().all(|message| {
        message
            .body
            .as_ref()
            .is_none_or(|body| body.len() <= max_bytes)
    });
    match within_count && within_bytes {
        true => Ok(messages.into_iter().map(Message::from_sdk).collect()),
        false => Err(batch_over_limit()),
    }
}
