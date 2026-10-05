//! The typed integration failure vocabulary, entered through the crate exports.
//!
//! Values only: an integration error is built through the doc-hidden driver
//! that runs the production error factory, so every closed kind, operation,
//! failure, and retryability is provable without a broker, a queue, or a DNS
//! provider. The driver cannot fabricate a lifecycle settlement; it builds the
//! error value and nothing else. The wire response those values produce belongs
//! to the acceptance root.

use camber::runtime_test_support::IntegrationErrorDriver;
use camber::{
    CleanupItem, IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation,
    Retryability, RuntimeError,
};
use std::error::Error;
use std::sync::Arc;

use crate::integration_vocabulary::{FAILURES, KINDS, OPERATIONS, RETRYABILITIES};
use crate::leaky_source::{LeakySource, SECRETS};

// ── The pairings each closed value is built with ─────────────────────

/// The advice one failure is built with in the failure rows.
///
/// Held to the spec's pairing rules rather than chosen per row: permission and
/// configuration refusals are `Never`, an unknown outcome is never `Safe`, and
/// `Safe` appears only where no side effect was submitted. No wildcard arm, so a
/// new failure needs a decision here before it has a row.
const fn advice_for(failure: IntegrationFailure) -> Retryability {
    match failure {
        IntegrationFailure::InvalidConfig
        | IntegrationFailure::PermissionDenied
        | IntegrationFailure::Rejected
        | IntegrationFailure::LimitExceeded
        | IntegrationFailure::Closed
        | IntegrationFailure::CleanupIncomplete
        | IntegrationFailure::InvalidCertificate => Retryability::Never,
        IntegrationFailure::Unavailable
        | IntegrationFailure::Busy
        | IntegrationFailure::Timeout
        | IntegrationFailure::Cancelled => Retryability::Safe,
        IntegrationFailure::OutcomeUnknown => Retryability::OutcomeUnknown,
    }
}

/// The failure one retryability is built with in the retryability rows.
const fn failure_for(retryability: Retryability) -> IntegrationFailure {
    match retryability {
        Retryability::Never => IntegrationFailure::InvalidConfig,
        Retryability::Safe => IntegrationFailure::Unavailable,
        Retryability::OutcomeUnknown => IntegrationFailure::OutcomeUnknown,
    }
}

/// The vocabulary crosses threads with the error that carries it.
const fn assert_shared<T: Send + Sync + 'static>() {}

const _: () = {
    assert_shared::<IntegrationError>();
    assert_shared::<CleanupItem>();
    assert_shared::<IntegrationKind>();
    assert_shared::<IntegrationOperation>();
    assert_shared::<IntegrationFailure>();
    assert_shared::<Retryability>();
};

/// One shared source, as the caller, settlement, and event would hold it.
fn leaky_source() -> Arc<dyn Error + Send + Sync> {
    Arc::new(LeakySource::every_secret())
}

/// The address a trait object's data lives at, for identity comparison.
fn data_address(error: &(dyn Error + 'static)) -> *const () {
    std::ptr::from_ref(error).cast::<()>()
}

/// Assert no secret reached one rendering.
fn assert_redacted(rendered: &str, label: &str) {
    for secret in SECRETS {
        assert!(
            !rendered.contains(secret),
            "{label}: the secret {secret:?} was rendered: {rendered}"
        );
    }
}

/// Assert `Display` and `Debug` of both the error and its runtime arm redact.
fn assert_all_renderings_redacted(error: &Arc<IntegrationError>, label: &str) {
    assert_redacted(&error.to_string(), &format!("{label}: Display"));
    assert_redacted(&format!("{error:?}"), &format!("{label}: Debug"));
    assert_redacted(&format!("{error:#?}"), &format!("{label}: alternate Debug"));
    let runtime = RuntimeError::Integration(Arc::clone(error));
    assert_redacted(
        &runtime.to_string(),
        &format!("{label}: RuntimeError Display"),
    );
    assert_redacted(
        &format!("{runtime:?}"),
        &format!("{label}: RuntimeError Debug"),
    );
}

/// Walk one error's source chain and report whether it reaches `expected`.
fn chain_reaches(error: &(dyn Error + 'static), expected: *const ()) -> bool {
    let mut current = error.source();
    while let Some(cause) = current {
        if data_address(cause) == expected {
            return true;
        }
        current = cause.source();
    }
    false
}

// ── The contract ──────────────────────────────────────────────────────

/// Every kind, operation, failure, and retryability survives the factory as
/// the value it was built with, and `cleanup` is empty outside cleanup failures.
fn assert_vocabulary_round_trips() -> usize {
    let mut rows = 0_usize;
    for kind in KINDS {
        let error = IntegrationErrorDriver::new(
            kind,
            IntegrationOperation::Connect,
            IntegrationFailure::Unavailable,
            Retryability::Safe,
        )
        .build();
        assert_eq!(error.kind(), kind, "{kind:?}: kind");
        rows += 1;
    }
    for operation in OPERATIONS {
        let error = IntegrationErrorDriver::new(
            IntegrationKind::Nats,
            operation,
            IntegrationFailure::Rejected,
            Retryability::Never,
        )
        .build();
        assert_eq!(error.operation(), operation, "{operation:?}: operation");
        rows += 1;
    }
    for failure in FAILURES {
        let error = IntegrationErrorDriver::new(
            IntegrationKind::Sqs,
            IntegrationOperation::Publish,
            failure,
            advice_for(failure),
        )
        .build();
        assert_eq!(error.failure(), failure, "{failure:?}: failure");
        assert_eq!(
            error.retryability(),
            advice_for(failure),
            "{failure:?}: retryability"
        );
        assert!(
            error.cleanup().is_empty(),
            "{failure:?}: an error built with no cleanup account carries none"
        );
        rows += 1;
    }
    for retryability in RETRYABILITIES {
        let error = IntegrationErrorDriver::new(
            IntegrationKind::Dns01,
            IntegrationOperation::Provision,
            failure_for(retryability),
            retryability,
        )
        .build();
        assert_eq!(
            error.retryability(),
            retryability,
            "{retryability:?}: retryability"
        );
        rows += 1;
    }
    rows
}

/// An instance identity is carried when admission assigned one, and absent
/// when the failure preceded admission.
fn assert_instance_identity() {
    let unadmitted = IntegrationErrorDriver::new(
        IntegrationKind::Sqs,
        IntegrationOperation::Connect,
        IntegrationFailure::InvalidConfig,
        Retryability::Never,
    )
    .build();
    assert_eq!(
        unadmitted.instance_id(),
        None,
        "a validation refusal before admission names no instance"
    );

    for id in [0_u64, 7, u64::MAX] {
        let admitted = IntegrationErrorDriver::new(
            IntegrationKind::Nats,
            IntegrationOperation::Publish,
            IntegrationFailure::Timeout,
            Retryability::Safe,
        )
        .instance(id)
        .build();
        assert_eq!(admitted.instance_id(), Some(id), "instance {id}");
    }
}

/// A cleanup failure carries every unresolved record as its own item, with the
/// exact provider ID when one was acknowledged and none for an unknown create.
fn assert_cleanup_account() {
    let error = IntegrationErrorDriver::new(
        IntegrationKind::Dns01,
        IntegrationOperation::DeleteTxt,
        IntegrationFailure::CleanupIncomplete,
        Retryability::Never,
    )
    .instance(3)
    .cleanup(
        "a.example.test",
        Some("rec-01"),
        IntegrationFailure::Rejected,
    )
    .cleanup("b.example.test", None, IntegrationFailure::OutcomeUnknown)
    .build();

    let items = error.cleanup();
    assert_eq!(items.len(), 2, "one item per unresolved record: {items:?}");
    assert_eq!(items[0].domain(), "a.example.test");
    assert_eq!(items[0].record_id(), Some("rec-01"));
    assert_eq!(items[0].failure(), IntegrationFailure::Rejected);
    assert_eq!(items[1].domain(), "b.example.test");
    assert_eq!(
        items[1].record_id(),
        None,
        "an unknown create has no acknowledged record ID"
    );
    assert_eq!(items[1].failure(), IntegrationFailure::OutcomeUnknown);
    assert_eq!(error.failure(), IntegrationFailure::CleanupIncomplete);
}

/// An SQS delete with an unknown outcome, built over one shared source.
fn unknown_delete_over(source: &Arc<dyn Error + Send + Sync>) -> IntegrationError {
    IntegrationErrorDriver::new(
        IntegrationKind::Sqs,
        IntegrationOperation::Delete,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
    .instance(11)
    .source(Arc::clone(source))
    .build()
}

/// One source is shared, not copied, across the caller's error, the settled
/// copy, and the runtime arm, and stays inspectable through `source`.
fn assert_shared_source() {
    let source = leaky_source();
    let expected = Arc::as_ptr(&source).cast::<()>();

    let caller = Arc::new(unknown_delete_over(&source));
    assert_eq!(
        Arc::strong_count(&source),
        2,
        "the factory holds the source it was handed, not a copy of it"
    );

    let settled = Arc::clone(&caller);
    let returned = RuntimeError::Integration(Arc::clone(&caller));
    assert_eq!(
        Arc::strong_count(&source),
        2,
        "sharing the error shares its source rather than copying it"
    );

    for (label, error) in [("caller", &caller), ("settlement", &settled)] {
        let cause = error
            .source()
            .unwrap_or_else(|| panic!("{label}: the SDK source stays inspectable"));
        assert_eq!(
            data_address(cause),
            expected,
            "{label}: the source is the one the adapter handed over"
        );
        assert!(
            cause.downcast_ref::<LeakySource>().is_some(),
            "{label}: the source keeps its concrete type"
        );
    }
    assert!(
        chain_reaches(&returned, expected),
        "the runtime arm's source chain reaches the shared SDK source"
    );

    let event_copy = unknown_delete_over(&source);
    assert_eq!(
        event_copy.source().map(data_address),
        Some(expected),
        "a second error built over the same source shares it too"
    );
    assert_eq!(Arc::strong_count(&source), 3);

    let without = IntegrationErrorDriver::new(
        IntegrationKind::Nats,
        IntegrationOperation::Subscribe,
        IntegrationFailure::Busy,
        Retryability::Safe,
    )
    .build();
    assert!(
        without.source().is_none(),
        "an error built without a source reports none"
    );
}

/// Neither rendering of any error repeats a secret its source carried.
fn assert_renderings_redact_every_secret() -> usize {
    let mut rows = 0_usize;
    for kind in KINDS {
        for failure in FAILURES {
            let error = Arc::new(
                IntegrationErrorDriver::new(
                    kind,
                    IntegrationOperation::Publish,
                    failure,
                    advice_for(failure),
                )
                .instance(5)
                .source(leaky_source())
                .build(),
            );
            assert_all_renderings_redacted(&error, &format!("{kind:?}/{failure:?}"));
            rows += 1;
        }
    }

    let cleanup = Arc::new(
        IntegrationErrorDriver::new(
            IntegrationKind::Dns01,
            IntegrationOperation::DeleteTxt,
            IntegrationFailure::CleanupIncomplete,
            Retryability::Never,
        )
        .source(leaky_source())
        .cleanup(
            "c.example.test",
            Some("rec-02"),
            IntegrationFailure::Timeout,
        )
        .build(),
    );
    assert_all_renderings_redacted(&cleanup, "cleanup account");
    for item in cleanup.cleanup() {
        assert_redacted(&format!("{item:?}"), "cleanup item Debug");
    }
    rows + 1
}

#[test]
fn integration_error_vocabulary_preserves_sources_and_redacts_secrets() {
    let vocabulary_rows = assert_vocabulary_round_trips();
    assert_eq!(
        vocabulary_rows,
        KINDS.len() + OPERATIONS.len() + FAILURES.len() + RETRYABILITIES.len(),
        "every closed value was built and read back"
    );

    assert_instance_identity();
    assert_cleanup_account();
    assert_shared_source();

    let redaction_rows = assert_renderings_redact_every_secret();
    assert_eq!(
        redaction_rows,
        KINDS.len() * FAILURES.len() + 1,
        "every kind and failure was rendered over a leaking source"
    );
}
