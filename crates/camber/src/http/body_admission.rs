//! Route-aware request-body admission.
//!
//! One matched route decides how much payload Camber may read, and whether the
//! application admits the work at all, before a single frame is polled. The
//! public contract here is the decision an application makes; everything else
//! in this module is the frozen configuration and the resolved plan the routing
//! stage hands to the concrete body consumer.
//!
//! The route selection that already chose the handler is the only authority
//! here. This module holds no path matcher and no host table: a plan can only
//! be built from a [`ResolvedRouteAuthority`], which the dispatch module mints
//! after its own selection has run.
//!
//! Every byte count a request is measured against is decided here too, and the
//! denial below is what keeps a declaration honest: a peer's stated length is a
//! `u64`, and narrowing one to the platform word is how a body no machine can
//! hold becomes a small admitted number.
#![deny(clippy::cast_possible_truncation)]

use super::dispatch::ResolvedRouteAuthority;
use super::mock::{LifecycleCheckpoint, LifecycleScript};
use super::rejection::{Rejected, RequestId};
use super::request::RequestHead;
use crate::RuntimeError;
use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

/// Default maximum request body size (8 MB).
const DEFAULT_MAX_BODY: usize = 8 * 1024 * 1024;
/// Hard ceiling for request body size (256 MB).
const MAX_BODY_LIMIT: usize = 256 * 1024 * 1024;

/// How a matched route consumes the payload bytes a peer sends.
///
/// A route that consumes no body — a WebSocket upgrade, an SSE stream, an
/// internal route, or a routing terminal — never invokes an application body
/// policy, so it needs no value here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestBodyMode {
    /// Bounded data is read into the owned [`Request`](super::Request) before
    /// the handler runs.
    Buffered,
    /// A bounded incoming stream is handed to the route's transport consumer.
    Streaming,
}

/// What one request established before Camber polled a payload frame.
///
/// A borrowed, synchronous view. It exposes no incoming body, no mutable
/// request state, no rejection policy, no private diagnostic, and no Hyper
/// type, and nothing it lends out can escape the policy call.
pub struct BodyAdmissionContext<'a> {
    request_id: RequestId,
    method: &'a str,
    raw_path: &'a str,
    route: &'a str,
    mode: RequestBodyMode,
    declared_length: Option<u64>,
    head: &'a RequestHead<'a>,
}

impl<'a> BodyAdmissionContext<'a> {
    /// Build the view one policy call is given.
    ///
    /// The declaration arrives already read, from the same single normalization
    /// the ceiling was checked against, rather than being derived again here:
    /// two readings are two answers that can disagree about what the peer
    /// declared.
    pub(super) fn new(
        head: &'a RequestHead<'a>,
        route: &'a str,
        mode: RequestBodyMode,
        declared_length: Option<u64>,
    ) -> Self {
        Self {
            request_id: head.request_id(),
            method: head.method_str(),
            raw_path: head.path(),
            route,
            mode,
            declared_length,
            head,
        }
    }

    /// The identity Camber minted for this request.
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// The method exactly as the peer sent it.
    pub fn method(&self) -> &str {
        self.method
    }

    /// The request path exactly as it arrived, before parameter binding.
    pub fn raw_path(&self) -> &str {
        self.raw_path
    }

    /// The normalized route pattern this request matched.
    pub fn route(&self) -> &str {
        self.route
    }

    /// How this route consumes payload bytes.
    pub fn mode(&self) -> RequestBodyMode {
        self.mode
    }

    /// The body length this request declared, after Hyper's framing
    /// normalization.
    ///
    /// `None` is a request with no usable declaration: an empty HTTP/1 body, a
    /// chunked body, or an HTTP/2 body whose size the peer never stated.
    pub fn declared_length(&self) -> Option<u64> {
        self.declared_length
    }

    /// One request header, under the same lookup
    /// [`Request::header`](super::Request::header) uses.
    ///
    /// Names are case-insensitive, the first value is selected, duplicates are
    /// not merged, and an absent, unrepresentable, or non-UTF-8 value is
    /// `None`.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head.header(name)
    }
}

/// The successful admission decision for one request body.
///
/// Not cloneable: the optional permit has exactly one owner, and a second
/// handle to it would be a second release.
pub struct BodyAdmission {
    max_bytes: usize,
    /// The application's permit, erased.
    ///
    /// Shared type erasure rather than a boxed trait object: this crate does
    /// not put `Box<dyn Trait>` in owned state, the public decision is
    /// non-generic, and the value must stay thread-safe because the request
    /// that holds it can move between runtime workers. Never handed out, never
    /// cloned, so the single handle here is the single release.
    permit: Option<Arc<dyn Any + Send + Sync>>,
}

impl BodyAdmission {
    /// Admit this request under a selected observed-byte maximum.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            permit: None,
        }
    }

    /// Admit this request and retain the supplied permit for its lifetime.
    ///
    /// The permit is dropped exactly once on every terminal path — completion,
    /// refusal, disconnect, cancellation alike — at the release point its mode
    /// names. A [`Buffered`](RequestBodyMode::Buffered) route releases it when
    /// the request holding it is released; a
    /// [`Streaming`](RequestBodyMode::Streaming) route releases it when the
    /// upload ends, drained or stopped or refused, which is before the upstream
    /// answers.
    ///
    /// `T: Send + Sync + 'static` is narrower than this crate's usual public
    /// bound because the permit travels with an HTTP request across runtime
    /// workers while its shared erasure must stay thread-safe.
    pub fn with_permit<T>(max_bytes: usize, permit: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        Self {
            max_bytes,
            permit: Some(Arc::new(permit)),
        }
    }

    /// The observed-byte maximum this decision selected.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Take the permit into the owner that holds it for the rest of the request.
    fn into_permit(self, observer: Option<&Arc<LifecycleScript>>) -> Option<BodyPermit> {
        self.permit.map(|permit| BodyPermit {
            _permit: permit,
            observer: observer.map(Arc::clone),
        })
    }
}

/// The single owner of one admitted request's application permit.
///
/// Held inside the request or the body consumer that the admission was made
/// for, so every terminal path — completion, refusal, disconnect, cancellation
/// — releases it by dropping one value. There is no release method, because a
/// second way to release is a second chance to release twice.
pub(super) struct BodyPermit {
    /// The erased application value, held only so that dropping this owner
    /// drops it. Underscored because nothing reads it and nothing may: reading
    /// it would need a downcast, and a downcast is a second way to reach a
    /// value that has exactly one owner.
    _permit: Arc<dyn Any + Send + Sync>,
    /// The listener-scoped observer that counts owners released, when a test
    /// registered one. Inert otherwise.
    observer: Option<Arc<LifecycleScript>>,
}

impl Drop for BodyPermit {
    fn drop(&mut self) {
        LifecycleScript::count_permit_owner_dropped(self.observer.as_deref());
    }
}

/// The application policy one router freezes.
pub(super) type BodyPolicy =
    dyn Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync;

/// Take one configured policy into the shape every stage reads it through.
///
/// One immutable shared allocation per router, because every concurrent request
/// that reaches that router calls the same closure — the ownership shape the
/// rejection mapper already uses.
pub(super) fn shared_policy<F>(policy: F) -> Arc<BodyPolicy>
where
    F: Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static,
{
    Arc::new(policy)
}

/// A router's configured request-body ceiling, and whether it was configured.
///
/// The distinction is load-bearing under host routing: a child that configured
/// nothing must inherit the host ceiling rather than narrow it to the default
/// eight MiB, and a child that configured one may only narrow it further.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum ConfiguredCeiling {
    /// Nothing was configured. The default applies, and any ceiling that
    /// contains this router wins.
    #[default]
    Unset,
    /// The application configured this ceiling, already clamped to the hard cap.
    Set(usize),
}

impl ConfiguredCeiling {
    /// Record a configured ceiling, clamped to the hard cap.
    pub(super) fn configured(bytes: usize) -> Self {
        Self::Set(bytes.min(MAX_BODY_LIMIT))
    }

    /// The bytes this ceiling admits with nothing containing it.
    fn bytes(self) -> usize {
        match self {
            Self::Unset => DEFAULT_MAX_BODY,
            Self::Set(bytes) => bytes,
        }
    }

    /// The bytes this ceiling admits inside the one containing it.
    ///
    /// `None` is a single router, which nothing contains. Under host routing an
    /// unset child inherits the containing ceiling outright, and a configured
    /// child can only narrow it.
    fn inside(self, outer: Option<Self>) -> usize {
        match outer {
            None => self.bytes(),
            Some(outer) => self.narrowed_by(outer),
        }
    }

    /// The bytes this ceiling admits under a ceiling that contains it.
    fn narrowed_by(self, outer: Self) -> usize {
        match self {
            Self::Unset => outer.bytes(),
            Self::Set(bytes) => bytes.min(outer.bytes()),
        }
    }
}

/// The immutable body policy one matched route resolved to.
///
/// Built once, by the routing stage, from the token that stage mints after its
/// own selection has run. Carried into the class that reads the wire, so the
/// route that chose the handler is the route that chose the limit.
pub(super) struct ResolvedBodyPlan {
    route: Arc<str>,
    mode: RequestBodyMode,
    ceiling: usize,
    policy: Option<Arc<BodyPolicy>>,
}

impl ResolvedBodyPlan {
    /// Resolve one matched route's body plan.
    ///
    /// The authority token is taken by reference and read for nothing: holding
    /// one is the proof that the existing trie and host resolver selected this
    /// route, and no other module can mint it.
    /// `outer` is the ceiling containing this router — a host router's own, or
    /// `None` for a single router. Resolving the precedence here keeps the one
    /// rule about how ceilings narrow in the module that owns limits.
    pub(super) fn new(
        _authority: &ResolvedRouteAuthority,
        route: Arc<str>,
        mode: RequestBodyMode,
        ceiling: ConfiguredCeiling,
        outer: Option<ConfiguredCeiling>,
        policy: Option<Arc<BodyPolicy>>,
    ) -> Self {
        Self {
            route,
            mode,
            ceiling: ceiling.inside(outer),
            policy,
        }
    }
}

/// The bounded read one admitted request was granted.
pub(super) struct AdmittedBody {
    /// The effective observed-byte maximum for this request.
    pub(super) limit: usize,
    /// The application permit this request retains, when one was supplied.
    pub(super) permit: Option<BodyPermit>,
}

/// Decide one body-consuming request's admission before any frame is polled.
///
/// The configured ceiling refuses a declaration before the policy is asked, so
/// an application never has to acquire a permit for a request its own
/// configuration has already ruled out.
///
/// The resolved limit is reported here, where the route's plan becomes this
/// request's bound, rather than at whichever consumer later reads under it.
/// Both modes enter through this one call, so the number a listener observes is
/// the one admission decided, and a refusal never reaches that line at all.
pub(super) async fn admit(
    plan: &ResolvedBodyPlan,
    head: &RequestHead<'_>,
    observer: Option<&Arc<LifecycleScript>>,
) -> Result<AdmittedBody, Rejected> {
    let admitted = match head.undecodable_transfer_coding() {
        Some(coding) => return Err(Rejected::undecodable_transfer_coding(coding)),
        None => admit_declared(plan, head, observer)?,
    };
    LifecycleScript::pause_at(
        observer.map(Arc::as_ref),
        LifecycleCheckpoint::RequestBodyLimitConfigured(admitted.limit),
    )
    .await;
    Ok(admitted)
}

/// Admit one request whose framing Camber can decode.
///
/// The declaration is read once, here, and handed to the policy stage rather
/// than re-derived by it: the ceiling check and the policy must not be able to
/// disagree about what the peer stated.
fn admit_declared(
    plan: &ResolvedBodyPlan,
    head: &RequestHead<'_>,
    observer: Option<&Arc<LifecycleScript>>,
) -> Result<AdmittedBody, Rejected> {
    let declared = head.declared_length();
    match declares_over(declared, plan.ceiling) {
        true => Err(Rejected::body_too_large(plan.ceiling)),
        false => admit_under_policy(plan, head, declared, observer),
    }
}

/// Admit under the route's configured policy, or under its ceiling alone.
///
/// The policy runs exactly once. Its selected maximum can only narrow the
/// configured ceiling, and a declaration above the narrowed maximum is refused
/// here — dropping the permit the policy just handed over, before any payload
/// is read.
fn admit_under_policy(
    plan: &ResolvedBodyPlan,
    head: &RequestHead<'_>,
    declared: Option<u64>,
    observer: Option<&Arc<LifecycleScript>>,
) -> Result<AdmittedBody, Rejected> {
    let policy = match &plan.policy {
        None => {
            return Ok(AdmittedBody {
                limit: plan.ceiling,
                permit: None,
            });
        }
        Some(policy) => policy,
    };
    let context = BodyAdmissionContext::new(head, &plan.route, plan.mode, declared);
    let admission = run_policy(policy, &context)?;
    let limit = admission.max_bytes().min(plan.ceiling);
    let permit = admission.into_permit(observer);
    match declares_over(declared, limit) {
        true => Err(Rejected::body_too_large(limit)),
        false => Ok(AdmittedBody { limit, permit }),
    }
}

/// Run one configured policy without letting its failure escape the request.
///
/// A panic is isolated because the policy runs before any request ownership
/// exists: an unwind escaping here would take the connection with it and leave
/// the peer with no answer at all. Neither failure re-enters the policy.
fn run_policy(
    policy: &Arc<BodyPolicy>,
    context: &BodyAdmissionContext<'_>,
) -> Result<BodyAdmission, Rejected> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| policy(context))) {
        Ok(Ok(admission)) => Ok(admission),
        Ok(Err(error)) => Err(Rejected::body_admission_refused(error)),
        Err(payload) => Err(Rejected::body_admission_panicked(payload)),
    }
}

/// Whether a declaration is above `limit`, compared at the declaration's width.
fn declares_over(declared: Option<u64>, limit: usize) -> bool {
    declared.is_some_and(|stated| declared_length_exceeds_limit(stated, limit))
}

/// Whether a peer's stated body length is above `limit`.
///
/// The comparison happens at the declaration's own width. Narrowing the stated
/// value to the platform word first is what turns a length no machine can hold
/// into a small admitted number, so the limit is widened instead. A limit that
/// does not fit a `u64` admits every declaration, which is the saturating
/// answer rather than a truncated one.
///
/// Pure: two integers in, one answer out, no allocation and no state.
#[doc(hidden)]
pub fn declared_length_exceeds_limit(declared: u64, limit: usize) -> bool {
    declared > u64::try_from(limit).unwrap_or(u64::MAX)
}

/// The running total after one decoded data frame, when the bound admits it.
///
/// `None` is the single answer to both ways a frame can fail its bound: it
/// would carry the request past the limit, or the addition that would prove
/// otherwise overflows. A caller cannot tell those apart because it must not
/// act differently — under either, this frame is never retained and never
/// forwarded.
///
/// Pure: three integers in, one answer out. It runs once per data frame of
/// every admitted body, so it allocates nothing and keeps no state of its own.
#[doc(hidden)]
pub fn checked_body_frame_total(observed: usize, frame_len: usize, limit: usize) -> Option<usize> {
    observed
        .checked_add(frame_len)
        .filter(|total| *total <= limit)
}

/// The checked bound one admitted request body is read under.
///
/// Both consumers measure through this: the buffered collector before it
/// appends a frame, and the streaming adapter before it forwards one. One
/// running total and one comparison, so a frame refused in one mode cannot be
/// admitted in the other.
pub(super) struct BodyBudget {
    limit: usize,
    observed: usize,
}

impl BodyBudget {
    /// Start a bound at the effective limit this admission resolved.
    ///
    /// Built from the decision rather than from a number a caller carried
    /// along: the bound a body is read under is the one its own admission
    /// selected, and a consumer that could name a different one would be a
    /// second authority over the same request.
    pub(super) fn new(admitted: &AdmittedBody) -> Self {
        Self {
            limit: admitted.limit,
            observed: 0,
        }
    }

    /// Account one decoded data frame, and answer with the new running total.
    ///
    /// Measured before the caller keeps or forwards the frame, so a frame that
    /// crosses the bound is refused while it is still only a value this call's
    /// caller holds. The refusal names the limit, never the transport: nothing
    /// went wrong with the wire.
    pub(super) fn admit_frame(&mut self, frame_len: usize) -> Result<usize, Rejected> {
        match checked_body_frame_total(self.observed, frame_len, self.limit) {
            Some(total) => {
                self.observed = total;
                Ok(total)
            }
            None => Err(Rejected::body_too_large(self.limit)),
        }
    }
}
