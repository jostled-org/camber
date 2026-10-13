//! The buffered terminal of one outbound operation.
//!
//! Every buffered call enters here: a verb convenience, a described request,
//! and a mocked call, with zero retries or with retries to spare. The call
//! polls all of its work inline. It spawns no task, so dropping the call drops
//! the attempt or the collection in flight and nothing can send afterwards.
//!
//! The shared coordinator returns the final head. This module collects that
//! head's body under the lifetime and the attempt timer the head carries.

use super::super::boundary::ByteBoundary;
use super::super::checked_collect::collect_checked;
use super::completion::{Completion, Terminal};
use super::description::Description;
use super::handoff::FinalHead;
use super::head::ResponseHead;
use super::intercept::{admit_recorded, mock_answer};
use super::lifetime::ATTEMPT_EXPIRED;
use super::operation::{final_head, first_poll};
use super::policy::OperationPolicy;
use super::projection::collection_failure;
use super::response::ClientResponse;
use super::reusable::Client;
use crate::RuntimeError;

/// Run one described call to its answer or its failure, and record its end.
///
/// This is the operation's first poll, and the record begins here. A call
/// that returns settles the record with the fact it ended on, after its
/// response and its transport are released. A call dropped while it is
/// pending leaves the record unsettled, and the record's own drop reports the
/// abandonment.
pub(super) async fn operate(
    client: &Client,
    description: Description,
    policy: OperationPolicy,
) -> Result<ClientResponse, RuntimeError> {
    let mut completion = Completion::begin(description.method(), description.url());
    let result = started(client, description, policy, &mut completion).await;
    completion.settle(Terminal::of(&result));
    result
}

/// Run one started call to its answer or its failure.
///
/// The checks that need more than one field run first, so a refused
/// description sends nothing. The executor is checked next, so a call polled
/// outside Tokio returns `NoRuntime` and arms no timer. The deadline is fixed
/// after that, and an expired one ends the call before a mock answers or the
/// network is used. The client's egress policy reads the scheme next, so a
/// refused scheme is refused under a mock too. A mock answers without a
/// lookup, and it is no evidence of where a connection would go. Only a call
/// the network answers reaches the coordinator: its target is resolved and
/// checked, its transport is chosen, and the caller's trace context is read,
/// in that order.
async fn started(
    client: &Client,
    mut description: Description,
    policy: OperationPolicy,
    completion: &mut Completion,
) -> Result<ClientResponse, RuntimeError> {
    description.admit()?;
    let (_, lifetime) = first_poll(client, &description, &policy)?;
    match mock_answer(&description) {
        Some(answer) => admit_recorded(answer, description.response_limit(), completion),
        None => {
            let transport = client
                .operation_transport(description.url(), &lifetime)
                .await?;
            description.carry_trace_context(policy.trace_context);
            collect(final_head(transport, description, policy, lifetime, completion).await?).await
        }
    }
}

/// Collect the final answer under the lifetime and the attempt timer its
/// head carries.
///
/// The head is copied first because the collector consumes the response. A
/// timer that expires here is the call's answer: the head holds no request,
/// so a final body is never sent for again. A refused or failed collection
/// returns its typed cause alone, and no partial answer reaches the caller.
///
/// No quiet interval of this collection's own is armed. A client's response
/// idle deadline is configured on its Reqwest client and enforced per read
/// there, so a second timer here would be a second owner of one dimension.
async fn collect(head: FinalHead<'_>) -> Result<ClientResponse, RuntimeError> {
    let FinalHead {
        response,
        lifetime,
        attempt,
        transport,
        limit,
    } = head;
    let stored = ResponseHead::of(&response);
    let read = collect_checked(response, ByteBoundary::ClientResponse, limit, None);
    let collected = lifetime.within_attempt(attempt, read).await;
    // The response is read or released. Its transport ends after it.
    drop(transport);
    match collected? {
        Some(Ok(body)) => Ok(ClientResponse::from_head(stored, body)),
        Some(Err(failure)) => Err(collection_failure(&failure)),
        None => Err(RuntimeError::Client(ATTEMPT_EXPIRED)),
    }
}
