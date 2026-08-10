//! The closed rejection taxonomy, bound to the enum that declares it.
//!
//! Two roots used to spell this list out: a focused root as bare
//! `RejectionKind` variants, and the metrics support as bare label strings.
//! Neither was tied to the enum, so a fifteenth variant compiled clean, both
//! lists went stale, and every assertion built on them went on passing against
//! a vocabulary that no longer covered the taxonomy.
//!
//! Two things are enforced here, and they are different claims. That the list
//! is complete is the production array's: [`KINDS`] IS `RejectionKind::ALL`, so
//! a new variant breaks the declared length where the variants are, and this
//! module has nothing left to keep in step. That every category has a bounded
//! name is [`label_of`]'s: its `match` has no wildcard arm, so the same variant
//! fails to compile until it is given one.
//!
//! Nothing in this module reaches another support module, so a root that needs
//! only the taxonomy mounts only this.

use camber::http::RejectionKind;

/// Every category the closed public taxonomy admits.
///
/// The production array itself, not a mirror of it. A hand-written copy was
/// what let the two drift: adding a variant forced a new [`label_of`] arm but
/// left the list one short, and `assert_closed_kind` went on passing while
/// covering one category fewer than the taxonomy had.
///
/// The list claims which categories exist, not which ones a journey happened to
/// reach. A category no fixture drives still belongs here, and one that gains a
/// producer needs no edit: `BodyAdmission` is raised by route-aware admission
/// refusal and asserted by its own fixtures, and the list carried it before
/// either existed.
pub const KINDS: [RejectionKind; RejectionKind::ALL.len()] = RejectionKind::ALL;

/// The bounded name one category is recorded and counted under.
///
/// The production spelling is `RejectionKind::label`, which is crate-private, so
/// a test that reads a scraped counter cannot call it. This is the mirror a test
/// reads through, and the mirror is deliberately a `match` with no `_` arm: a
/// new variant fails here until it is named, which is what keeps the recorded
/// vocabulary as wide as the taxonomy [`KINDS`] already carries.
pub const fn label_of(kind: RejectionKind) -> &'static str {
    match kind {
        RejectionKind::Routing => "routing",
        RejectionKind::MethodSelection => "method_selection",
        RejectionKind::BodyLimit => "body_limit",
        RejectionKind::BodyAdmission => "body_admission",
        RejectionKind::BodyUnreadable => "body_unreadable",
        RejectionKind::BodyTimeout => "body_timeout",
        RejectionKind::MalformedBody => "malformed_body",
        RejectionKind::Multipart => "multipart",
        RejectionKind::InvalidHeader => "invalid_header",
        RejectionKind::Application => "application",
        RejectionKind::Middleware => "middleware",
        RejectionKind::WebSocketHandshake => "websocket_handshake",
        RejectionKind::Proxy => "proxy",
        RejectionKind::InternalService => "internal_service",
    }
}
