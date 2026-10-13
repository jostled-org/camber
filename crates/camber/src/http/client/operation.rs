//! The final-head coordinator of one outbound operation: every attempt, every
//! followed redirect, and every delay under one lifetime.
//!
//! Every call the network answers enters here, buffered or streamed, with
//! zero retries or with retries to spare. The coordinator polls its work
//! inside its caller's future. It spawns no task, so dropping that future
//! drops the attempt in flight and nothing can send afterwards. A buffered
//! call is that future. A streamed call runs it inside the one task that owns
//! its response.
//!
//! The coordinator ends at the final head. It returns that head with the
//! lifetime, the attempt timer, and the transport it arrived under, and it
//! drops the request. No retry and no redirect can follow an accepted head,
//! because the value that could send is gone.
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
use super::error::{ClientError, ClientRedirect};
use super::exchange::Exchange;
use super::handoff::FinalHead;
use super::lifetime::{ATTEMPT_EXPIRED, Lifetime};
use super::policy::OperationPolicy;
use super::projection::client_error_projection;
use super::redirect::{
    Hop, client_next_redirect, client_redirect_destination, client_send_total, redirect_carried,
    redirect_location,
};
use super::replay::{client_retryable_failure, client_retryable_status};
use super::reusable::Client;
use super::transport::OperationTransport;
use crate::RuntimeError;
use crate::runtime;
use reqwest::Url;
use std::time::Duration;
use tokio::time::Instant;

/// Run the checks every terminal's first poll shares, in their one order.
///
/// The executor is checked first, so a call polled outside Tokio returns
/// `NoRuntime` and arms no timer. A stop request is read next. The deadline is
/// fixed after that, and an expired one ends the call before a mock answers
/// or the network is used. The client's egress policy reads the scheme last,
/// so a refused scheme is refused under a mock too.
pub(super) fn first_poll(
    client: &Client,
    description: &Description,
    policy: &OperationPolicy,
) -> Result<(tokio::runtime::Handle, Lifetime), RuntimeError> {
    let executor = tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
    runtime::check_cancel()?;
    let lifetime = Lifetime::begin(policy.total, policy.deadline)?;
    client.admit_scheme(description.url())?;
    Ok((executor, lifetime))
}

/// Run one admitted call to its final response head.
///
/// The caller chose `transport` for this call's target and terminal, and the
/// description already holds the trace context the call began under.
/// `lifetime` is the one the call's first poll fixed. The returned head
/// carries it on, with the transport: one pinned to this call's checked
/// addresses is released when the head is.
///
/// `completion` is borrowed until the head is final. The coordinator reports
/// each send, each followed redirect, and the final status to it, and the
/// borrow ends here.
pub(super) async fn final_head<'t>(
    transport: OperationTransport<'t>,
    description: Description,
    policy: OperationPolicy,
    lifetime: Lifetime,
    completion: &mut Completion,
) -> Result<FinalHead<'t>, RuntimeError> {
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

/// What the operation does with one response head.
enum Answered {
    /// Hand it on: it is the call's answer.
    Final,
    /// Release it and send the current request again as this retry.
    Retry(u32),
    /// Release it, then follow the redirect it asks for or end with the
    /// refusal.
    Redirect(Result<Hop, ClientRedirect>),
}

/// The uniquely owned coordinator of one call's attempts.
///
/// `'t` is the borrow of the client a reusable transport reads. `'c` is the
/// borrow of the terminal record. They are separate so that the final head
/// keeps the first and cannot keep the second.
struct Operation<'t, 'c> {
    /// The URL the call began at. Its origin is the only one a redirect may
    /// name, and no hop changes it.
    initial: Url,
    /// The current request: the first one, or the one the last followed
    /// redirect asked for.
    exchange: Exchange<'t>,
    policy: OperationPolicy,
    lifetime: Lifetime,
    /// How many retries this operation has sent, across every hop.
    retries_sent: u32,
    /// How many redirects this operation has followed, across every retry.
    redirects_followed: usize,
    /// The terminal record. The operation reports each send, each followed
    /// redirect, and its final head, and reads nothing back.
    completion: &'c mut Completion,
}

impl<'t> Operation<'t, '_> {
    /// Run attempts until one answer is final, a failure is not replayable, a
    /// redirect is refused, or the operation ends.
    ///
    /// Each attempt has one timer, armed at its send. The final attempt's
    /// timer leaves with its head and is held through the end of its body.
    /// Each send after the first spends one configured retry or one redirect
    /// hop.
    async fn run(mut self) -> Result<FinalHead<'t>, RuntimeError> {
        loop {
            let bound = self.lifetime.attempt_bound(self.policy.attempt);
            let head = self.head(bound).await?;
            match head.map(|resp| (self.answered(&resp), resp)) {
                Ok((Answered::Final, resp)) => return Ok(self.accept(resp, bound)),
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
    /// refused redirect dials nothing. The destination has the initial origin:
    /// its scheme is the one the egress policy admitted, and its host is the
    /// one a pinned transport holds checked addresses for. A followed redirect
    /// therefore starts no lookup.
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

    /// Accept one head as the call's answer, and end the coordinator.
    ///
    /// The head is final from here, whatever reading its body returns, so its
    /// status is recorded first. The request is dropped: the returned head
    /// holds the transport and the response maximum and nothing to send. The
    /// lifetime and the attempt timer move on as they are, so neither one
    /// starts again.
    fn accept(self, response: reqwest::Response, attempt: Option<Instant>) -> FinalHead<'t> {
        self.completion.final_head(response.status().as_u16());
        let (transport, limit) = self.exchange.finish();
        FinalHead {
            response,
            lifetime: self.lifetime,
            attempt,
            transport,
            limit,
        }
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
