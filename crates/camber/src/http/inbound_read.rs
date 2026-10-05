//! One admitted body, read under every bound its operation carries.

use super::body_admission::BodyBudget;
use super::operation::{
    InboundFailure, InboundGuard, InboundReady, InboundTerminal, OperationEnvelope, OperationStage,
};
use super::rejection::Rejected;

/// Retain one admitted body's data frames under every bound its operation
/// carries.
///
/// One turn of this loop reads every inbound source together — the shutdown
/// and cancellation authority the envelope carries, the two request deadlines,
/// the peer's own response lifetime, and whatever the wire produced. What this
/// reader read for itself is what it commits: the frame the wire produced, the
/// maximum that frame crossed, the peer's own end, and the payload's end are
/// one read, and the carried sources decide only a turn that read decided
/// nothing in. Two carried sources that first became observable in the same
/// turn may commit in either order, and nothing is polled after a terminal is
/// selected.
///
/// Every frame is accounted through the shared bound first and appended second,
/// so a frame that would carry this request past its limit is dropped while it
/// is still only a value this loop holds — the buffer never grows past what the
/// route admitted.
///
/// What is reported is the buffer's own length, read after the append. The
/// bound's running sum only ever names a total it already admitted, so a case
/// asking what this request held would be shown the arithmetic restating itself
/// rather than the bytes.
pub(super) async fn retain_within_budget(
    mut body: hyper::body::Incoming,
    budget: BodyBudget,
    operation: &OperationEnvelope,
    script: Option<&super::mock::LifecycleScript>,
) -> Result<bytes::Bytes, InboundFailure> {
    operation.observe(script, OperationStage::Body);
    let mut guard = operation.inbound();
    let mut budget = budget;
    let mut retained = bytes::BytesMut::new();
    let mut read = InboundRead {
        budget: &mut budget,
        retained: &mut retained,
        guard: &mut guard,
        script,
    };
    loop {
        super::mock::LifecycleScript::pause_at_response_commit(
            script,
            super::mock::ResponseCommitmentEdge::BeforeResponseCommit,
        )
        .await;
        let (ready, wire) = advance(&mut body, &mut read).await;
        let Some(terminal) = ready.first_ready() else {
            continue;
        };
        // The payload's end commits nothing: it releases the owner that will
        // produce this operation's head, and that owner takes the commitment
        // where it produces it. Every other terminal is a cause, and the cause
        // only maps if this reader is the first owner to reach the cell.
        let committed = match terminal {
            InboundTerminal::ResponseHead => None,
            ended => Some(operation.commitment().commit_cause(ended)),
        };
        super::mock::LifecycleScript::pause_at_settled_commit(
            script,
            committed.and_then(|attempt| attempt.ok().map(|()| terminal)),
        )
        .await;
        return match (terminal, committed) {
            (InboundTerminal::ResponseHead, _) => Ok(std::mem::take(read.retained).freeze()),
            (ended, Some(Ok(()))) => Err(InboundFailure::of(ended, operation.budget(), wire)),
            // Another producer holds this operation's answer, so this reader
            // maps nothing. The payload it retained is not an answer either:
            // what it owes is to stop reading, which is what a silent ending is.
            (ended, Some(Err(_)) | None) => Err(InboundFailure::silent(ended)),
        };
    }
}

/// The state one inbound turn reads and writes.
///
/// Bundled because the turn is one decision over all of it: the bound that
/// admits a frame, the buffer that keeps it, the authority that can end the
/// read, and the observer that records what happened move together or not at
/// all.
struct InboundRead<'a> {
    budget: &'a mut BodyBudget,
    retained: &'a mut bytes::BytesMut,
    guard: &'a mut InboundGuard,
    script: Option<&'a super::mock::LifecycleScript>,
}

/// Advance one inbound turn, and re-collect every source it ended with.
///
/// The carried sources are read again after the wait rather than before it: a
/// deadline that expired while a frame was in flight belongs to the same turn
/// as the frame, and weighing it against a stale reading would answer this turn
/// with an observation the turn never made.
async fn advance(
    body: &mut hyper::body::Incoming,
    read: &mut InboundRead<'_>,
) -> (InboundReady, Option<Rejected>) {
    use http_body_util::BodyExt;
    // The wire is offered first, so a frame already waiting is read into the
    // same turn as the deadlines beside it. Deciding on the deadlines alone
    // would let a body that already crossed its route's byte maximum be
    // reported as an idle expiry — a carried source answering a turn this
    // reader's own read of the wire had already decided.
    let frame = tokio::select! {
        biased;
        frame = body.frame() => Some(frame),
        () = read.guard.quiet() => None,
    };
    let carried = read.guard.observed();
    match frame {
        None => (carried, None),
        Some(frame) => fold(frame, read, carried),
    }
}

/// Fold one wire read into the turn's ready set.
///
/// The three wire outcomes stay apart. A limit is the bound's own answer, a
/// transport fault is the wire's, and the payload's end is neither; collapsing
/// them told a peer to shrink a request that was the right size.
fn fold(
    frame: Option<Result<hyper::body::Frame<bytes::Bytes>, hyper::Error>>,
    read: &mut InboundRead<'_>,
    carried: InboundReady,
) -> (InboundReady, Option<Rejected>) {
    let frame = match frame {
        None => return (carried.with_response_head(), None),
        Some(Ok(frame)) => frame,
        Some(Err(error)) => {
            return (
                carried.with_source_failure(),
                Some(Rejected::body_unreadable(error)),
            );
        }
    };
    // Trailers carry no payload, so they are neither counted nor measured, and
    // they renew no quiet interval.
    let Ok(data) = frame.into_data() else {
        return (carried, None);
    };
    super::mock::LifecycleScript::count_body_frame(read.script);
    match read.budget.admit_frame(data.len()) {
        Err(rejected) => (carried.with_route_body_limit(), Some(rejected)),
        Ok(_) => {
            read.retained.extend_from_slice(&data);
            super::mock::LifecycleScript::observe_body_retained(read.script, read.retained.len());
            read.guard.frame_delivered(data.len());
            (carried, None)
        }
    }
}
