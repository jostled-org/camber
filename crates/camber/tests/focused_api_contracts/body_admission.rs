//! The public body-admission value contract.
//!
//! Entered through the exported `camber::http` types alone: what an application
//! can build, what it can read back, what it must not have to clone, and when
//! the permit it handed over is released.

#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
use camber::__private::checked_body_frame_total;
use camber::__private::declared_length_exceeds_limit;
use camber::http::{BodyAdmission, RequestBodyMode};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A permit whose release is the only thing it records.
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// Compile-time proof that a value may cross runtime workers.
fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn body_admission_values_expose_limits_and_drop_generic_permits_once() {
    assert_send_sync::<BodyAdmission>();
    assert_send_sync::<RequestBodyMode>();

    // The one distinction a policy branches on. Nothing here pins the derived
    // `Debug` text: how a mode prints is not something an application decides
    // anything from.
    assert_ne!(RequestBodyMode::Buffered, RequestBodyMode::Streaming);

    let empty = BodyAdmission::new(0);
    assert_eq!(
        empty.max_bytes(),
        0,
        "a zero admission selects a zero maximum"
    );
    // Borrowed, never consumed: reading the decision twice must not require a
    // clone the permit could not survive.
    assert_eq!(empty.max_bytes(), 0);
    drop(empty);

    let drops = Arc::new(AtomicUsize::new(0));
    let admission = BodyAdmission::with_permit(usize::MAX, DropProbe(Arc::clone(&drops)));
    assert_eq!(admission.max_bytes(), usize::MAX);
    assert_eq!(
        drops.load(Ordering::Acquire),
        0,
        "the permit is retained while the decision that holds it is alive"
    );
    drop(admission);
    assert_eq!(
        drops.load(Ordering::Acquire),
        1,
        "one decision owner releases the permit exactly once"
    );
}

#[test]
fn body_admission_has_no_clone_impl() {
    // Two impls, one blanket and one constrained to `Clone`. The compiler can
    // resolve the marker for `BodyAdmission` only while no `Clone`
    // implementation exists; adding one makes this call ambiguous and this file
    // stops compiling.
    trait AmbiguousIfClone<Witness> {
        fn admits_one_owner() -> bool {
            true
        }
    }
    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: ?Sized + Clone> AmbiguousIfClone<u8> for T {}

    assert!(
        <BodyAdmission as AmbiguousIfClone<_>>::admits_one_owner(),
        "the admission decision resolves the unconstrained marker, so it is not Clone"
    );
}

/// What one accounting call answered, held for the assertions outside the
/// measured window.
///
/// Formatting an assertion message allocates, so nothing is asserted while the
/// counter is running. Every answer is collected first and read afterwards.
///
/// Built only where the allocation probe is, for the reason
/// [`checked_body_frame_total_rejects_overflow_without_allocation`] states.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
struct FrameTotals {
    below: Option<usize>,
    equal: Option<usize>,
    crossing: Option<usize>,
    exact_fill: Option<usize>,
    overflowing: Option<usize>,
    saturated_limit: Option<usize>,
    empty_frame_at_limit: Option<usize>,
}

/// Prove the allocation probe can see one allocation before it is trusted to
/// report none.
///
/// A counter that observed nothing would make every zero below meaningless.
///
/// Built only where the allocation probe is, for the reason
/// [`checked_body_frame_total_rejects_overflow_without_allocation`] states.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
fn assert_allocation_probe_calibrated() {
    let calibration = allocation_counter::measure(|| {
        drop(std::hint::black_box(Box::new(1_u32)));
    });
    assert!(
        calibration.count_total > 0,
        "the allocation probe must observe a known allocation before proving absence"
    );
}

/// Call the production accounting operation across every edge, once, inside a
/// single measured window.
///
/// The answers come back rather than being asserted here: formatting an
/// assertion message allocates, and an assertion inside the window would be
/// counting the test harness instead of the operation.
///
/// Built only where the allocation probe is, for the reason
/// [`checked_body_frame_total_rejects_overflow_without_allocation`] states.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
fn measure_frame_totals() -> (FrameTotals, u64) {
    let mut totals = FrameTotals {
        below: None,
        equal: None,
        crossing: None,
        exact_fill: None,
        overflowing: None,
        saturated_limit: None,
        empty_frame_at_limit: None,
    };
    let measured = allocation_counter::measure(|| {
        totals.below = checked_body_frame_total(
            std::hint::black_box(4),
            std::hint::black_box(8),
            std::hint::black_box(64),
        );
        totals.equal = checked_body_frame_total(
            std::hint::black_box(56),
            std::hint::black_box(8),
            std::hint::black_box(64),
        );
        totals.crossing = checked_body_frame_total(
            std::hint::black_box(60),
            std::hint::black_box(8),
            std::hint::black_box(64),
        );
        totals.exact_fill = checked_body_frame_total(
            std::hint::black_box(0),
            std::hint::black_box(0),
            std::hint::black_box(0),
        );
        totals.overflowing = checked_body_frame_total(
            std::hint::black_box(usize::MAX),
            std::hint::black_box(1),
            std::hint::black_box(usize::MAX),
        );
        totals.saturated_limit = checked_body_frame_total(
            std::hint::black_box(usize::MAX),
            std::hint::black_box(0),
            std::hint::black_box(usize::MAX),
        );
        totals.empty_frame_at_limit = checked_body_frame_total(
            std::hint::black_box(64),
            std::hint::black_box(0),
            std::hint::black_box(64),
        );
    });
    (totals, measured.count_total)
}

/// What the per-frame accounting operation costs, measured rather than read off
/// the source.
///
/// `allocation-counter` owns the counting `GlobalAlloc`, so it is referenced
/// only when Camber leaves the process allocator alone: `jemalloc` and
/// `mimalloc` each install their own, and two global allocators do not link.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
#[test]
fn checked_body_frame_total_rejects_overflow_without_allocation() {
    assert_allocation_probe_calibrated();
    let (totals, allocations) = measure_frame_totals();

    assert_eq!(
        totals.below,
        Some(12),
        "a frame inside the bound answers with the new running total"
    );
    assert_eq!(
        totals.equal,
        Some(64),
        "a frame that exactly fills the bound is admitted"
    );
    assert_eq!(
        totals.crossing, None,
        "a frame that would cross the bound is refused"
    );
    assert_eq!(
        totals.exact_fill,
        Some(0),
        "an empty frame under a zero limit is admitted"
    );
    assert_eq!(
        totals.overflowing, None,
        "an addition that overflows is refused rather than wrapped into a small total"
    );
    assert_eq!(
        totals.saturated_limit,
        Some(usize::MAX),
        "an addition that does not overflow at the widest limit still answers"
    );
    assert_eq!(
        totals.empty_frame_at_limit,
        Some(64),
        "an empty frame at the bound neither advances the total nor crosses it"
    );
    assert_eq!(
        allocations, 0,
        "the per-frame accounting operation allocates nothing"
    );
}

#[test]
fn declared_length_comparison_preserves_u64_width() {
    assert!(
        !declared_length_exceeds_limit(0, 0),
        "an empty declaration fits a zero limit"
    );
    assert!(
        declared_length_exceeds_limit(1, 0),
        "any stated byte is above a zero limit"
    );
    assert!(
        !declared_length_exceeds_limit(1023, 1024),
        "a declaration below the limit is admitted"
    );
    assert!(
        !declared_length_exceeds_limit(1024, 1024),
        "a declaration equal to the limit is admitted"
    );
    assert!(
        declared_length_exceeds_limit(1025, 1024),
        "a declaration above the limit is refused"
    );
    // The edge a premature `declared as usize` erases. On a 64-bit host the
    // cast is lossless and this row still passes, which is why the module-level
    // truncation denial is the other half of this proof: the narrowing
    // implementation fails the canonical Clippy lane rather than this assert.
    assert!(
        declared_length_exceeds_limit(u64::MAX, 1024),
        "a declaration no machine can hold stays above a small limit"
    );
    assert!(
        !declared_length_exceeds_limit(u64::MAX, usize::MAX),
        "the widest declaration fits a limit that saturates to it"
    );
    assert!(
        !declared_length_exceeds_limit(u64::from(u32::MAX), usize::MAX),
        "a limit wider than the declaration admits it"
    );
}
