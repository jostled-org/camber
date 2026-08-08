//! What a rejection counter is named, read as, and allowed to carry.
//!
//! Two roots scrape `http_rejections_total`: the component matrix that drives
//! every category with a producer, and the operator journey that reads what a
//! real service reported. The counter names, the paired read, the vocabulary
//! and the check are stated once here, because two copies of them are two
//! things that can drift apart — and the copy that drifts is the one that
//! quietly stops proving anything.

use super::http::HttpResponse;
use super::metrics_scrape::{Sample, delta, delta_by, scraped_samples};
use super::rejection_kinds::{KINDS, label_of};

/// The counter one refusal is counted under.
///
/// Private, because [`rejection_counters`] is the only way this module hands a
/// counter out: a root that named the metric itself would be free to scrape a
/// different one and still call the answer a rejection count.
const REJECTION_METRIC: &str = "http_rejections_total";

/// The counter one completed request is counted under.
const COMPLETION_METRIC: &str = "http_requests_total";

/// The two counters one rejection journey reads, scraped together.
pub struct RejectionCounters {
    pub rejections: Box<[Sample]>,
    pub completions: Box<[Sample]>,
}

/// Read both counters out of one scrape.
///
/// One body carries both, so one send takes both. Two sends read two moments:
/// the first scrape is itself a completed request, so `http_requests_total` had
/// already counted it by the time the second body reported that counter, and
/// every completion delta taken across the pair carried a request no row drove.
/// The order the two are read in is no longer a claim — one snapshot has no
/// order — which is why `send` is now spent once.
///
/// The request stays with its root: one root reads the endpoint through the
/// component client and its wire timeout, the other through the journey's own
/// send. Which counters are read, and that the answer was a `200` before it was
/// parsed, are stated here.
pub fn rejection_counters(send: impl FnOnce() -> HttpResponse) -> RejectionCounters {
    let scrape = send();
    RejectionCounters {
        rejections: scraped_samples(&scrape, REJECTION_METRIC),
        completions: scraped_samples(&scrape, COMPLETION_METRIC),
    }
}

/// Every category the rejection counter is allowed to name.
///
/// Derived from the enum rather than spelled out: a bare list of names is a list
/// nothing binds to the taxonomy, so a fifteenth `RejectionKind` compiles clean
/// while every assertion here goes on passing against a vocabulary that no
/// longer covers it. `label_of` has no wildcard arm, so that variant fails to
/// compile instead.
///
/// `body_admission` is reserved for route-aware admission control and has no
/// producer today, so no fixture generates it; it belongs here anyway, because
/// the claim this set makes is which names are permitted, not which ones a
/// journey happened to reach.
///
/// Private: [`assert_bounded_rejection_labels`] is the only reader, and a root
/// holding the set could compare a scraped name against it without the two
/// checks that make the comparison mean anything.
const CLOSED_REJECTION_KINDS: [&str; KINDS.len()] = closed_labels();

/// [`CLOSED_REJECTION_KINDS`], evaluated where `[T; N]::map` cannot be.
///
/// `map` takes a `FnMut` and is not a `const fn`, so the one expression that
/// says "every declared kind, under its own label" is written as the indexed
/// walk a const context admits.
const fn closed_labels() -> [&'static str; KINDS.len()] {
    let mut labels = [""; KINDS.len()];
    let mut index = 0;
    while index < KINDS.len() {
        labels[index] = label_of(KINDS[index]);
        index += 1;
    }
    labels
}

/// The only labels a rejection counter may carry, declared in sorted order.
///
/// The order is part of the declaration: [`assert_closed_label_names`] sorts
/// only the names it scraped and compares them against this as it stands, so a
/// pair added here out of order would fail every sample.
///
/// Private, like the vocabulary it sits beside: the ordering rule holds only
/// inside the comparison below, so the value is of no use to a reader outside
/// it.
const REJECTION_LABELS: [&str; 2] = ["kind", "status"];

/// Assert every scraped rejection sample carries only the closed vocabulary.
///
/// Three claims, and all three have to hold of the same sample: the label names
/// are exactly the closed pair, the category is one the taxonomy declares, and
/// no label value carries anything one request could vary. An empty scrape
/// satisfies all three vacuously, so it is refused first: both callers drive at
/// least one counted refusal before reading, and a slice with nothing in it
/// means the counter was renamed or the scrape failed to parse.
pub fn assert_bounded_rejection_labels(samples: &[Sample], forbidden: &[&str]) {
    assert!(!samples.is_empty(), "no rejection samples were scraped");
    for sample in samples {
        assert_closed_label_names(sample);
        assert_closed_kind(sample);
        assert_bounded_values(sample, forbidden);
    }
}

/// Assert one sample is labelled with exactly the closed pair of names.
fn assert_closed_label_names(sample: &Sample) {
    let labels = sample.labels();
    assert_eq!(
        labels.len(),
        REJECTION_LABELS.len(),
        "the rejection sample {sample:?} carries {} labels rather than the closed pair {REJECTION_LABELS:?}",
        labels.len()
    );
    // Scrape order is the recorder's, not the taxonomy's, so the names are put
    // in order before they are compared. The buffer is exactly the closed pair
    // wide, which the length above has just established.
    let mut names = [""; REJECTION_LABELS.len()];
    for (slot, (name, _)) in names.iter_mut().zip(labels) {
        *slot = name.as_ref();
    }
    names.sort_unstable();
    assert_eq!(
        names, REJECTION_LABELS,
        "the rejection sample {sample:?} carries labels outside the closed set"
    );
}

/// Assert one sample names a category the taxonomy declares.
fn assert_closed_kind(sample: &Sample) {
    let kind = sample
        .label("kind")
        .unwrap_or_else(|| panic!("the rejection sample {sample:?} names no category"));
    assert!(
        CLOSED_REJECTION_KINDS.contains(&kind),
        "the rejection sample {sample:?} names the category {kind:?}, which is outside the closed set"
    );
}

/// Assert no label value on one sample carries an unbounded value.
///
/// The forbidden list is refused when it is empty for the same reason the sample
/// list is: every caller builds it from a request identifier, a peer address,
/// and the paths its own rows drove, so a list with nothing in it means that
/// build produced nothing and the check would pass over any value at all.
fn assert_bounded_values(sample: &Sample, forbidden: &[&str]) {
    assert!(
        !forbidden.is_empty(),
        "no unbounded values were declared, so no rejection label can be checked against them"
    );
    for (_, value) in sample.labels() {
        assert!(
            !forbidden.iter().any(|text| value.contains(text)),
            "the rejection sample {sample:?} carries the unbounded value {value:?}"
        );
    }
}

/// Everything a rejection label must never carry, taken from one journey.
///
/// The identifier and the peer address are always in it: they are the two
/// high-cardinality values every journey produces, and a label check that
/// omitted either would be checking a vocabulary against nothing that could
/// break it. `extra` carries the messages and private causes a journey states
/// for itself, and `paths` the targets its rows drove.
///
/// Borrowed rather than owned: every value here already lives as long as the
/// assertion that reads it, so a copy per journey would buy nothing.
pub fn forbidden_values<'a>(
    request_id: &'a str,
    extra: &[&'a str],
    paths: impl Iterator<Item = &'a str>,
) -> Box<[&'a str]> {
    [request_id, PEER_ADDRESS]
        .into_iter()
        .chain(extra.iter().copied())
        .chain(paths)
        .collect()
}

/// The address every local peer in these journeys connects from.
const PEER_ADDRESS: &str = "127.0.0.1";

/// How many declared rows satisfy one predicate.
///
/// Generic over the row, because both journeys ask the same question of tables
/// that share no type: how many of the rows I declared should have moved this
/// counter. Answering it per root left two spellings of one conversion, and the
/// conversion is the part that can be got wrong.
pub fn counted_rows<T>(rows: &[T], matching: impl Fn(&T) -> bool) -> u64 {
    let count = rows.iter().filter(|row| matching(row)).count();
    u64::try_from(count).expect("a declared row count fits a counter value")
}

/// What one row's refusal must have done to both counters.
///
/// Stated as one value because the two deltas are one claim: the peer received
/// one outcome, and the rejection counter and the completion counter both have
/// to name it. A row asserted against only the first would hold for a refusal
/// no request ever completed under.
pub struct CountedOutcome<'a> {
    pub label: &'a str,
    /// The category the refusal was counted under.
    pub kind: &'a str,
    /// The status the peer was actually given.
    pub status: u16,
    /// How many refusals the declared rows raise under that category and status.
    pub rejections: u64,
    /// How many requests the declared rows complete under that status.
    pub completions: u64,
}

/// Assert one row moved both counters to the outcome its peer was given.
///
/// The rejection counter is read under the row's own category as well as its
/// status: a journey that refuses at two different stages proves nothing about
/// either if both are read as one number. A row the counter is silent for is
/// measured the same way, against a declared zero, so the silence is asserted
/// rather than inherited from a label pair nobody looked at.
pub fn assert_counted(
    before: &RejectionCounters,
    after: &RejectionCounters,
    expected: &CountedOutcome<'_>,
) {
    let label = expected.label;
    let status = expected.status.to_string();
    assert_eq!(
        delta(
            &before.rejections,
            &after.rejections,
            &[("kind", expected.kind), ("status", status.as_str())]
        ),
        expected.rejections,
        "{label}: the rejection counter names the status the peer was given"
    );
    assert_eq!(
        delta_by(
            &before.completions,
            &after.completions,
            "status",
            status.as_str()
        ),
        expected.completions,
        "{label}: the completion counter agrees with it"
    );
}
