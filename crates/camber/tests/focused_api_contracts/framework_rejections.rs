//! The public rejection contract, entered through the exported `http` types.
//!
//! Values only: a rejection, its context, and a request identifier are all
//! stack or owned data, so the whole taxonomy is provable without a socket.
//! The wire behaviour those values produce belongs to the component and
//! acceptance roots.

use crate::rejection_kinds::KINDS;

use camber::RuntimeError;
use camber::http::{
    NegotiatedResponseMetadata, Rejection, RejectionContext, RejectionKind, RejectionProtocol,
    Request, RequestId, Response,
};

/// A taxonomy with no categories would drive no row and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed. The
/// list itself is the shared one, bound to the enum through a `match` with no
/// wildcard arm — so a variant added without a decision about its safe
/// projection fails to compile rather than leaving this test covering one
/// category fewer than the taxonomy holds.
const _: () = assert!(!KINDS.is_empty());

/// The statuses a handshake refusal is built with.
const HANDSHAKE_STATUSES: [u16; 3] = [400, 403, 426];

/// A table with no rows would build no rejection and still report success.
const _: () = assert!(!HANDSHAKE_STATUSES.is_empty());

/// The statuses an internal-service refusal is built with.
const INTERNAL_STATUSES: [u16; 2] = [500, 503];

/// The same claim for the second table, which owes its own count.
const _: () = assert!(!INTERNAL_STATUSES.is_empty());

/// One generated identifier, taken through the public accessor.
///
/// `Request::builder` mints a detached identity through the production
/// generator, so a focused test reads the same value a served request would
/// without a second identity model.
fn generated_id() -> RequestId {
    Request::builder()
        .path("/items")
        .finish()
        .expect("the fixture target is an accepted request target")
        .request_id()
}

/// Assert one minted identifier's shape: 32 digits, and lowercase hexadecimal.
///
/// Both halves are the claim. A caller that checked the length alone would
/// accept digits no generator produces, and one that checked the alphabet alone
/// would accept an identifier of any width.
///
/// Local because this root mounts the rejection-kind list and nothing else, so
/// the support helper that states the same pair is out of reach. Three sites
/// wrote it out, and the third checked only the length.
fn assert_id_shape(id: &str) {
    assert_eq!(id.len(), 32, "an identifier renders as 32 digits: {id:?}");
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "an identifier renders as lowercase hexadecimal: {id:?}"
    );
}

/// The signature a configured mapper must satisfy.
///
/// Taking a value here is the compile-time half of the redaction claim: a
/// mapper is offered a borrowed rejection and a borrowed context, and there is
/// no third parameter through which a diagnostic could arrive.
fn accepts_rejection_mapper<F>(_mapper: F)
where
    F: Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static,
{
}

#[test]
fn public_rejection_contract_exposes_only_client_safe_state() {
    let mut rows = 0_usize;
    for kind in KINDS {
        let rejection =
            Rejection::new(kind, 400, "safe detail").expect("400 is a representable status");
        assert_eq!(rejection.kind(), kind);
        assert_eq!(rejection.status(), 400);
        assert_eq!(rejection.message(), "safe detail");
        assert_eq!(rejection.headers().count(), 0);
        assert_eq!(
            rejection,
            Rejection::new(kind, 400, "safe detail").expect("400 is a representable status"),
            "two rejections built from the same data compare equal"
        );
        assert!(
            format!("{rejection:?}").contains(&format!("{kind:?}")),
            "the debug projection names its category"
        );
        rows += 1;
    }
    assert_eq!(rows, KINDS.len(), "every declared category was exercised");

    assert_ne!(
        Rejection::new(RejectionKind::Routing, 404, "not found")
            .expect("404 is a representable status"),
        Rejection::new(RejectionKind::Application, 404, "not found")
            .expect("404 is a representable status"),
        "the category is part of a rejection's identity, not its status alone"
    );
}

#[test]
fn rejection_status_is_data_rather_than_a_function_of_its_category() {
    let mut rows = 0_usize;
    for status in HANDSHAKE_STATUSES {
        let rejection = Rejection::new(RejectionKind::WebSocketHandshake, status, "refused")
            .expect("every handshake status is representable");
        assert_eq!(rejection.status(), status);
        assert_eq!(rejection.kind(), RejectionKind::WebSocketHandshake);
        rows += 1;
    }
    assert_eq!(rows, HANDSHAKE_STATUSES.len());

    let mut internal_rows = 0_usize;
    for status in INTERNAL_STATUSES {
        let rejection = Rejection::new(RejectionKind::InternalService, status, "service state")
            .expect("every internal status is representable");
        assert_eq!(rejection.status(), status);
        assert_eq!(rejection.kind(), RejectionKind::InternalService);
        internal_rows += 1;
    }
    assert_eq!(internal_rows, INTERNAL_STATUSES.len());

    assert!(
        Rejection::new(RejectionKind::Routing, 42, "unrepresentable").is_err(),
        "a status outside 100-599 cannot become a rejection"
    );
}

#[test]
fn rejection_headers_are_borrowed_in_registration_order() {
    let rejection = Rejection::new(RejectionKind::MethodSelection, 405, "method not allowed")
        .expect("405 is a representable status")
        .with_header("Allow", "GET, HEAD")
        .with_header("X-Safe", "yes");

    let headers: Vec<(&str, &str)> = rejection.headers().collect();
    assert_eq!(headers, [("Allow", "GET, HEAD"), ("X-Safe", "yes")]);
    assert_eq!(rejection.message(), "method not allowed");
}

#[test]
fn rejection_context_reports_absence_only_through_option() {
    let id = generated_id();

    let bare = RejectionContext::new(id, "GET", "/items");
    assert_eq!(bare.request_id(), &id);
    assert_eq!(bare.method(), "GET");
    assert_eq!(bare.raw_path(), "/items");
    assert_eq!(bare.remote_addr(), None);
    assert_eq!(bare.route(), None);
    assert!(bare.negotiated().is_none());

    let remote: std::net::IpAddr = "203.0.113.9".parse().expect("a literal IPv4 address");
    let established = RejectionContext::new(id, "PATCH", "/users/7")
        .with_remote_addr(remote)
        .with_route("/users/:id")
        .with_negotiated(
            NegotiatedResponseMetadata::new(RejectionProtocol::WebSocket).with_subprotocol("chat"),
        );

    assert_eq!(established.remote_addr(), Some(remote));
    assert_eq!(established.route(), Some("/users/:id"));
    let negotiated = established
        .negotiated()
        .expect("the fixture established negotiated metadata");
    assert_eq!(negotiated.protocol(), RejectionProtocol::WebSocket);
    assert_eq!(negotiated.subprotocol(), Some("chat"));
    assert_eq!(
        negotiated.content_type(),
        None,
        "an unestablished value stays absent rather than becoming an empty sentinel"
    );
}

#[test]
fn request_id_display_and_accessor_agree_on_32_lowercase_hex_digits() {
    let id = generated_id();

    assert_eq!(id.to_string(), id.as_str());
    assert_id_shape(id.as_str());
    assert_eq!(id, id, "a copied identifier keeps value equality");
    assert_ne!(
        id,
        generated_id(),
        "two generated identifiers name two requests"
    );
}

#[test]
fn mapper_signature_admits_only_borrowed_safe_inputs() {
    accepts_rejection_mapper(|rejection: &Rejection, context: &RejectionContext| {
        Response::text(rejection.status(), rejection.message())
            .map(|response| response.with_header("X-Request-Id", context.request_id().as_str()))
    });
}

/// What identifier generation and access cost, measured rather than read off
/// the source.
///
/// `allocation-counter` owns the counting `GlobalAlloc`, so it is referenced
/// only when Camber leaves the process allocator alone: `jemalloc` and
/// `mimalloc` each install their own, and two global allocators do not link.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
#[test]
fn request_id_generation_and_access_are_allocation_free() {
    // Outside every measured window: the generator's process nonce and the
    // thread-local seed behind it are established once, and a window that
    // paid for that establishment would be measuring initialization.
    let warmup = camber::http::mock::generated_request_id();
    assert_id_shape(warmup.as_str());

    let calibration = allocation_counter::measure(|| {
        drop(std::hint::black_box(Box::new(1_u32)));
    });
    assert!(
        calibration.count_total > 0,
        "a probe that counts nothing would make every zero below meaningless"
    );

    let mut generated = Vec::with_capacity(64);
    let generation = allocation_counter::measure(|| {
        for _ in 0..64 {
            std::hint::black_box(camber::http::mock::generated_request_id());
        }
    });
    assert_eq!(
        generation.count_total, 0,
        "generating an identifier writes fixed inline storage and allocates nothing"
    );

    let observed = camber::http::mock::generated_request_id();
    let access = allocation_counter::measure(|| {
        for _ in 0..64 {
            std::hint::black_box(observed.as_str());
        }
    });
    assert_eq!(
        access.count_total, 0,
        "reading an identifier borrows its inline digits"
    );

    for _ in 0..64 {
        generated.push(camber::http::mock::generated_request_id());
    }
    for id in &generated {
        assert_id_shape(id.as_str());
    }
    let mut unique: Vec<&str> = generated.iter().map(RequestId::as_str).collect();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        generated.len(),
        "every generated identifier names one request"
    );
}
