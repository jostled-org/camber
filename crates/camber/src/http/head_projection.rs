//! The head a gated route's middleware is shown, and what it added to it.
//!
//! A specialized route — a stream, an event source, an upgrade, a forwarded
//! payload, or a gRPC call — has no response head while its chain runs: the head
//! is an upstream's, a producer's, or tonic's, and none of them has answered
//! yet. The chain is shown a provisional one instead, and what it states over
//! that provisional head is kept here until the real head exists.
//!
//! Two things make that safe. The metadata is validated where it is stated, so a
//! header the wire cannot carry refuses the request before an upstream, a
//! producer, or an upgrade begins. And the merge never displaces what a protocol
//! decided for itself: a name the real head already carries is the protocol's
//! own correction, and it stands.

use super::body::HyperResponseBody;
use super::rejection::{Rejected, RejectionScope};
use super::response::{HeaderPair, Response};

/// The status a gate terminal answers with before a real head exists.
///
/// Documented as provisional rather than chosen per class: the classes that run
/// a gate learn their real status from an upstream, a producer, or tonic, and a
/// chain shown anything else would be reading a status no peer will be given.
pub(super) const PROVISIONAL_STATUS: u16 = 200;

/// The successful metadata one middleware chain stated over a provisional head.
///
/// Held as a validated [`hyper::HeaderMap`] rather than as the response's own
/// pairs: validation happens once, where the chain stated the metadata, and the
/// merge into a real head is then infallible. Grouping by name is what lets the
/// merge answer "does the protocol already own this name" once per name while
/// still carrying every value a chain stated under a name it owns alone.
pub(super) struct HeadProjection {
    headers: hyper::HeaderMap,
}

impl HeadProjection {
    /// Nothing was projected: no chain ran, or the one that ran stated nothing.
    pub(super) fn none() -> Self {
        Self {
            headers: hyper::HeaderMap::new(),
        }
    }

    /// Validate what a passed-through chain stated over the provisional head.
    ///
    /// The failure is the builder's own, so the refusal it becomes names exactly
    /// what could not be represented.
    fn captured(headers: &[HeaderPair]) -> Result<Self, hyper::http::Error> {
        let mut projected = hyper::HeaderMap::with_capacity(headers.len());
        for (name, value) in headers {
            projected.append(
                hyper::header::HeaderName::try_from(name.as_ref())?,
                hyper::header::HeaderValue::from_str(value.as_ref())?,
            );
        }
        Ok(Self { headers: projected })
    }

    /// Merge this metadata into the real head its protocol committed.
    ///
    /// A name the head already carries is the protocol's own correction and
    /// stands: the merge adds, and never replaces. A name the protocol does not
    /// speak for arrives with every value the chain stated under it, so a chain
    /// that set two cookies still sets two.
    pub(super) fn merged_into(
        &self,
        mut answered: hyper::Response<HyperResponseBody>,
    ) -> hyper::Response<HyperResponseBody> {
        for name in self.headers.keys() {
            if answered.headers().contains_key(name) {
                continue;
            }
            for value in self.headers.get_all(name) {
                answered.headers_mut().append(name.clone(), value.clone());
            }
        }
        answered
    }
}

/// What one gate decided about a request.
///
/// Two outcomes, not a success and a failure: a chain that answered is a request
/// already served, and a chain that passed carries forward the only thing it
/// produced. Generic over what an answer is spelled as, because each stage owns
/// that spelling — a chain's own [`Response`] before it reaches the wire, and a
/// built hyper response after.
pub(super) enum Gated<T> {
    /// The chain admitted the request, and stated this over its head.
    Admitted(HeadProjection),
    /// The chain answered instead, and no specialized work may begin.
    Answered(T),
}

/// Read what one gate chain's answer decided.
///
/// A pass is read off the response's own provenance rather than a flag the
/// terminal sets: a frame that replaces the terminal's value — with a refusal,
/// or with a body of its own — has answered, and a reached-flag cannot tell that
/// from a pass. That is what keeps a replacement body from being mistaken for
/// permission to begin a handoff.
///
/// Metadata the wire cannot carry is refused here, under this request's own
/// policy. It is the last point before specialized work begins, and a head that
/// could never have been built must not first start an upstream leg, a producer,
/// or an upgrade.
pub(super) fn decided(answered: Response, scope: &RejectionScope) -> Gated<Response> {
    match answered.provenance().is_gate_passthrough() {
        false => Gated::Answered(answered),
        true => admitted(answered.headers(), scope),
    }
}

/// Keep what a passed-through chain stated, or refuse what cannot be carried.
fn admitted(stated: &[HeaderPair], scope: &RejectionScope) -> Gated<Response> {
    match HeadProjection::captured(stated) {
        Ok(projection) => Gated::Admitted(projection),
        Err(error) => Gated::Answered(scope.map(Rejected::unrepresentable(error))),
    }
}
