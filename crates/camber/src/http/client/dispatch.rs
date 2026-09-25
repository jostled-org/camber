//! Where one call goes: a registered mock, a single attempt, or a retry
//! sequence.

use super::super::method::Method as LocalMethod;
use super::super::{Response, mock};
use super::exchange::Exchange;
use super::sequence::{RetryPlan, RetrySequence};
use crate::RuntimeError;
use crate::runtime;
use reqwest::Method;
use reqwest::header::HeaderValue;

/// What one dispatch runs under.
///
/// The transport, the retry plan, and the ceiling the answer is collected
/// under travel together because they are one configuration: a retry that
/// reused a different client, or an answer collected under a different
/// maximum, would be a second policy for the same call.
#[derive(Clone, Copy)]
pub(super) struct Dispatch<'a> {
    client: &'a reqwest::Client,
    retry: Option<RetryPlan>,
    /// The response ceiling, or `None` for the named opt-out.
    response_limit: Option<usize>,
}

impl<'a> Dispatch<'a> {
    pub(super) fn new(
        client: &'a reqwest::Client,
        retry: Option<RetryPlan>,
        response_limit: Option<usize>,
    ) -> Self {
        Self {
            client,
            retry,
            response_limit,
        }
    }

    /// Answer one call from a mock if one is registered, otherwise from the
    /// network under this dispatch's configuration.
    pub(super) async fn run(
        self,
        method: Method,
        url: &str,
        body: Option<(HeaderValue, &str)>,
    ) -> Result<Response, RuntimeError> {
        runtime::check_cancel()?;
        match try_mock(&method, url) {
            Some(resp) => Ok(resp),
            None => {
                self.send(Exchange::new(
                    self.client,
                    &method,
                    url,
                    body,
                    self.response_limit,
                ))
                .await
            }
        }
    }

    /// Send one exchange once, or under a retry sequence when retries are
    /// configured.
    async fn send(self, exchange: Exchange<'_>) -> Result<Response, RuntimeError> {
        match self.retry {
            None => exchange.once().await,
            Some(plan) => RetrySequence::begin(plan, exchange).run().await,
        }
    }
}

fn try_mock(method: &Method, url: &str) -> Option<Response> {
    let local_method = LocalMethod::from_reqwest(method);
    local_method.and_then(|m| mock::try_intercept(m, url))
}
