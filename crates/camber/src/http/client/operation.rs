//! One complete outbound operation: every attempt, every followed redirect,
//! every delay, and the final collection under one lifetime.
//!
//! Every call enters here: a verb convenience, a described request, and a
//! mocked call, with zero retries or with retries to spare. The operation
//! polls all of its work inline. It spawns no task, so dropping the call drops
//! the attempt in flight and nothing can send afterwards.
//!
//! A per-attempt boundary that commits before the response head keeps its own
//! result and enters the replay decision. The operation deadline and runtime
//! shutdown end the operation and drop whatever was in flight.
//!
//! The operation holds one current request and two counters. A retry repeats
//! the current request and spends one retry. A followed redirect replaces the
//! current request and spends one hop. Neither counter is reset by the other,
//! so the sends of one call never exceed its first plus both allowances.

use super::completion::{Completion, method_label};
use super::delay::{retry_delay, stated_retry_after};
use super::description::Description;
use super::error::{ClientError, ClientRedirect, ClientTimeout};
use super::exchange::Exchange;
use super::intercept::{admit_mock, mock_answer};
use super::lifetime::Lifetime;
use super::policy::OperationPolicy;
use super::projection::client_error_projection;
use super::redirect::{
    Hop, client_next_redirect, client_redirect_destination, client_send_total, redirect_carried,
    redirect_location,
};
use super::replay::{client_retryable_failure, client_retryable_status};
use super::response::ClientResponse;
use super::reusable::Client;
use crate::RuntimeError;
use crate::runtime;
use reqwest::Url;
use std::time::Duration;
use tokio::time::Instant;

/// Run one described call to its answer or its failure, and record its end.
///
/// This is the operation's first poll, and the record begins here. A call
/// that returns settles the record with its result. A call dropped while it
/// is pending leaves the record unsettled, and the record's own drop reports
/// the abandonment.
pub(super) async fn operate(
    client: &Client,
    description: Description,
    policy: OperationPolicy,
) -> Result<ClientResponse, RuntimeError> {
    let mut completion = Completion::begin(description.method(), description.url());
    let result = started(client, description, policy, &mut completion).await;
    completion.settle(&result);
    result
}

/// Run one started call to its answer or its failure.
///
/// The checks that need more than one field run first, so a refused
/// description sends nothing. The executor is checked next, so a call polled
/// outside Tokio returns `NoRuntime` and arms no timer. The deadline is fixed
/// after that, and an expired one ends the call before a mock answers or the
/// network is used. The transport is built, and the caller's trace context is
/// read, only when the network is used.
async fn started(
    client: &Client,
    mut description: Description,
    policy: OperationPolicy,
    completion: &mut Completion,
) -> Result<ClientResponse, RuntimeError> {
    description.admit()?;
    tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
    runtime::check_cancel()?;
    let lifetime = Lifetime::begin(policy.total, policy.deadline)?;
    match mock_answer(&description) {
        Some(answer) => {
            completion.final_head(answer.status());
            admit_mock(answer, description.response_limit())
        }
        None => {
            let transport = client.transport()?;
            description.carry_trace_context(policy.trace_context);
            Operation {
                initial: description.url().clone(),
                exchange: Exchange::new(transport, description),
                policy,
                lifetime,
                retries_sent: 0,
                redirects_followed: 0,
                completion,
            }
            .run()
            .await
        }
    }
}

/// The category of an attempt whose timer expired, before its head or during
/// its body.
const ATTEMPT_EXPIRED: ClientError = ClientError::Timeout(ClientTimeout::Attempt);

/// What the operation does with one response head.
enum Answered {
    /// Collect it: it is the call's answer.
    Final,
    /// Release it and send the current request again as this retry.
    Retry(u32),
    /// Release it, then follow the redirect it asks for or end with the
    /// refusal.
    Redirect(Result<Hop, ClientRedirect>),
}

/// The uniquely owned coordinator of one call's attempts.
struct Operation<'a> {
    /// The URL the call began at. Its origin is the only one a redirect may
    /// name, and no hop changes it.
    initial: Url,
    /// The current request: the first one, or the one the last followed
    /// redirect asked for.
    exchange: Exchange<'a>,
    policy: OperationPolicy,
    lifetime: Lifetime,
    /// How many retries this operation has sent, across every hop.
    retries_sent: u32,
    /// How many redirects this operation has followed, across every retry.
    redirects_followed: usize,
    /// The terminal record. The operation reports each send, each followed
    /// redirect, and its final head, and reads nothing back.
    completion: &'a mut Completion,
}

impl Operation<'_> {
    /// Run attempts until one answer is final, a failure is not replayable, a
    /// redirect is refused, or the operation ends.
    ///
    /// Each attempt has one timer, armed at its send and held through the end
    /// of its body. Each send after the first spends one configured retry or
    /// one redirect hop.
    async fn run(mut self) -> Result<ClientResponse, RuntimeError> {
        loop {
            let bound = self.lifetime.attempt_bound(self.policy.attempt);
            let head = self.head(bound).await?;
            match head.map(|resp| (self.answered(&resp), resp)) {
                Ok((Answered::Final, resp)) => return self.collect(resp, bound).await,
                Ok((Answered::Retry(retry), resp)) => self.retry_status(resp, retry).await?,
                Ok((Answered::Redirect(decision), resp)) => self.follow(resp, decision)?,
                Err(failure) => self.retry_failure(failure).await?,
            }
        }
    }

    /// Decide what one response head asks of the operation.
    ///
    /// A redirect is read first. The statuses a redirect is followed for and
    /// the statuses a retry is earned by are disjoint, so the order chooses
    /// between no two outcomes.
    fn answered(&self, resp: &reqwest::Response) -> Answered {
        match (self.redirect(resp), self.next_retry()) {
            (Some(decision), _) => Answered::Redirect(decision),
            (None, Some(retry)) if self.replays_status(resp) => Answered::Retry(retry),
            (None, _) => Answered::Final,
        }
    }

    /// The redirect one response head asks for, decided, or `None` when the
    /// head is not a redirect this request follows.
    fn redirect(&self, resp: &reqwest::Response) -> Option<Result<Hop, ClientRedirect>> {
        let status = resp.status().as_u16();
        redirect_location(self.policy.redirect, status, resp.headers())
            .map(|location| location.and_then(|location| self.hop(status, location)))
    }

    /// Admit one redirect: its destination, then its method, then its hop.
    ///
    /// Every check runs before a request to the destination exists, so a
    /// refused redirect dials nothing.
    fn hop(&self, status: u16, location: &[u8]) -> Result<Hop, ClientRedirect> {
        let url = client_redirect_destination(&self.initial, self.exchange.url(), location)?;
        let carried = redirect_carried(status, self.exchange.method(), self.policy.permission)?;
        let ordinal = client_next_redirect(self.policy.redirect, self.redirects_followed)
            .filter(|hop| client_send_total(self.retries_sent, *hop).is_some())
            .ok_or(ClientRedirect::HopLimit)?;
        Ok(Hop {
            url,
            carried,
            ordinal,
        })
    }

    /// Release a redirect answer, then make its authorized hop the current
    /// request or end the call with its refusal.
    ///
    /// The answer is dropped unread in both cases, which closes its
    /// connection: a redirect body is never drained, and a refused one is not
    /// held past the refusal. No delay follows, and the next send starts under
    /// the operation's own deadline.
    fn follow(
        &mut self,
        resp: reqwest::Response,
        decision: Result<Hop, ClientRedirect>,
    ) -> Result<(), RuntimeError> {
        let status = resp.status().as_u16();
        drop(resp);
        let hop =
            decision.map_err(|refusal| RuntimeError::Client(ClientError::Redirect(refusal)))?;
        tracing::debug!(
            method = method_label(self.exchange.method()),
            host = self.exchange.host(),
            status,
            redirect = hop.ordinal,
            "following same-origin HTTP redirect"
        );
        self.exchange.follow(hop.url, hop.carried);
        self.redirects_followed = hop.ordinal;
        self.completion.followed();
        Ok(())
    }

    /// Release a transient answer, then wait to send `retry`.
    async fn retry_status(
        &mut self,
        resp: reqwest::Response,
        retry: u32,
    ) -> Result<(), RuntimeError> {
        let delay = self.dispose_transient(resp, retry);
        self.wait_to_retry(delay, retry).await
    }

    /// Wait to send the retry a failed attempt earns, or return the failure
    /// as the call's answer.
    async fn retry_failure(&mut self, failure: ClientError) -> Result<(), RuntimeError> {
        match self.next_retry() {
            Some(retry) if self.replays_failure(failure) => {
                let delay = self.note_failure(failure, retry);
                self.wait_to_retry(delay, retry).await
            }
            Some(_) | None => Err(RuntimeError::Client(failure)),
        }
    }

    /// Wait out the delay before `retry`, then count it as sent.
    async fn wait_to_retry(&mut self, delay: Duration, retry: u32) -> Result<(), RuntimeError> {
        self.lifetime.wait(delay).await?;
        self.retries_sent = retry;
        Ok(())
    }

    /// Send one attempt and wait for its response head.
    ///
    /// The outer error is the operation's end. The inner one is the category
    /// of this attempt's own failure, which the replay decision reads. The
    /// transport's error is projected once and released here. The send is
    /// counted when the transport is given it, so an operation that ended
    /// first counts no send.
    async fn head(
        &mut self,
        bound: Option<Instant>,
    ) -> Result<Result<reqwest::Response, ClientError>, RuntimeError> {
        let (exchange, completion) = (&self.exchange, &mut *self.completion);
        let sent = self
            .lifetime
            .within_attempt(bound, async {
                completion.sent();
                exchange.send().await
            })
            .await?;
        Ok(match sent {
            Some(Ok(resp)) => Ok(resp),
            Some(Err(error)) => Err(client_error_projection(&error)),
            None => Err(ATTEMPT_EXPIRED),
        })
    }

    /// Collect the final answer under the attempt timer its send armed.
    ///
    /// A timer that expires here is the call's answer. A final body is never
    /// sent for again. The head is final from here, whatever its collection
    /// returns, so its status is recorded first.
    async fn collect(
        &mut self,
        resp: reqwest::Response,
        bound: Option<Instant>,
    ) -> Result<ClientResponse, RuntimeError> {
        self.completion.final_head(resp.status().as_u16());
        self.lifetime
            .within_attempt(bound, self.exchange.collect(resp))
            .await?
            .unwrap_or(Err(RuntimeError::Client(ATTEMPT_EXPIRED)))
    }

    /// The ordinal of the retry a replay would send, or `None` when the
    /// configured count is spent.
    ///
    /// The addition is checked, so a count of `u32::MAX` ends at its last
    /// retry instead of wrapping to a fresh allowance. A retry the send count
    /// cannot hold is not sent.
    fn next_retry(&self) -> Option<u32> {
        self.retries_sent
            .checked_add(1)
            .filter(|retry| *retry <= self.policy.retries)
            .filter(|retry| client_send_total(*retry, self.redirects_followed).is_some())
    }

    fn replays_status(&self, resp: &reqwest::Response) -> bool {
        client_retryable_status(
            resp.status().as_u16(),
            self.exchange.method(),
            self.policy.permission,
        )
    }

    fn replays_failure(&self, failure: ClientError) -> bool {
        client_retryable_failure(failure, self.exchange.method(), self.policy.permission)
    }

    /// Release a transient answer before the delay begins, and choose that
    /// delay.
    ///
    /// Dropping the unread response closes its connection now, so the peer is
    /// not held for the length of the backoff.
    fn dispose_transient(&self, resp: reqwest::Response, retry: u32) -> Duration {
        let status = resp.status().as_u16();
        let stated = stated_retry_after(&resp);
        drop(resp);
        tracing::debug!(
            method = method_label(self.exchange.method()),
            host = self.exchange.host(),
            status,
            attempt = retry,
            "retrying transient HTTP status"
        );
        retry_delay(self.policy.backoff, self.retries_sent, stated)
    }

    /// Record a replayable failed attempt, and choose the delay after it.
    ///
    /// The event names the method label, the host, and the failure's category.
    /// It never names the path, the query, or the transport's own text.
    fn note_failure(&self, failure: ClientError, retry: u32) -> Duration {
        tracing::debug!(
            method = method_label(self.exchange.method()),
            host = self.exchange.host(),
            category = %failure,
            attempt = retry,
            "retrying transient HTTP error"
        );
        retry_delay(self.policy.backoff, self.retries_sent, None)
    }
}
