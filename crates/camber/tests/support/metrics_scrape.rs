//! One Prometheus scrape, read as the samples it declares.
//!
//! Two roots assert on the same live counters — the component observability
//! root and the core-acceptance operator journeys — so the text format is
//! parsed once here. A second copy is a second thing that can disagree about
//! what a label set means.

/// One counter line: the labels it carries and the value it reports.
///
/// Counters only, so the value is whole: a scrape that reports a fractional
/// value for a counter is a scrape this reader has no meaning for, and it fails
/// rather than rounding one in.
///
/// `Debug` is what every rule this sample can break reports it under: a
/// vocabulary check that failed without naming the sample tells an operator a
/// rule was broken and leaves them to find which line broke it.
#[derive(Debug)]
pub struct Sample {
    labels: Box<[(Box<str>, Box<str>)]>,
    value: u64,
}

impl Sample {
    /// The value this sample reports.
    ///
    /// Private: [`value_where`] is the only reader, and every delta this module
    /// hands out is a sum over a selection. A caller holding one sample's value
    /// would be reading a counter one label set at a time, which is the reading
    /// [`delta_by`] exists so that nobody has to do.
    fn value(&self) -> u64 {
        self.value
    }

    /// The value one label carries, when the sample carries that label.
    pub fn label(&self, name: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(candidate, _)| candidate.as_ref() == name)
            .map(|(_, value)| value.as_ref())
    }

    /// Every label this sample carries, in scrape order.
    ///
    /// Borrowed rather than collected: both readers of it only walk the pairs,
    /// so a fresh boxed copy per sample would be an allocation neither keeps.
    pub fn labels(&self) -> &[(Box<str>, Box<str>)] {
        &self.labels
    }

    /// Whether this sample carries exactly `labels`, in any order.
    fn matches(&self, labels: &[(&str, &str)]) -> bool {
        labels.len() == self.labels.len()
            && labels
                .iter()
                .all(|(name, value)| self.label(name) == Some(*value))
    }
}

/// Every sample one scraped response reports for `metric`.
///
/// The `200` and the parse are one step because they are one claim: samples read
/// off a body nothing checked the status of would report an empty counter for an
/// endpoint that answered `404`, and every delta taken from it would then be
/// zero for a reason no assertion names. The request itself stays with the
/// calling root — one reads the endpoint through the component client and its
/// wire timeout, the other through the journey's own send — because that is the
/// one thing the two roots legitimately differ in.
pub fn scraped_samples(response: &super::http::HttpResponse, metric: &str) -> Box<[Sample]> {
    assert_eq!(
        response.status, 200,
        "the metrics endpoint answered {} rather than 200",
        response.status
    );
    samples(&String::from_utf8_lossy(&response.body), metric)
}

/// Every sample one scrape reports for `metric`.
///
/// Private, because [`scraped_samples`] is the whole claim: samples read off a
/// body nothing checked the status of report an empty counter for an endpoint
/// that answered `404`. A caller reaching the parse on its own would be able to
/// skip that check, which is the one thing pairing them was for.
fn samples(scrape: &str, metric: &str) -> Box<[Sample]> {
    scrape
        .lines()
        .filter_map(|line| parse_sample(line, metric))
        .collect()
}

/// Read one scrape line as a sample of `metric`, or skip it.
///
/// Two questions, kept apart because conflating them is what lets a sample
/// vanish. Whether the line is this metric's at all is decided first, by the
/// name and by what follows it: a line for another metric that merely begins
/// with the same name — the histogram's `_sum` beside its counter — continues
/// into a suffix where this reader requires a label set or a value, so it is
/// skipped. Once the line IS this metric's, its value must read; a value that
/// will not parse is a scrape this reader has no meaning for, and it fails
/// rather than dropping a sample the caller asked for and reporting zero.
fn parse_sample(line: &str, metric: &str) -> Option<Sample> {
    let rest = line.strip_prefix(metric)?;
    let (labels, value) = match rest.strip_prefix('{') {
        Some(labelled) => labelled.split_once('}')?,
        None if rest.starts_with(char::is_whitespace) => ("", rest),
        None => return None,
    };
    let value = value.trim();
    Some(Sample {
        labels: parse_labels(labels),
        value: value
            .parse()
            .unwrap_or_else(|_| panic!("a {metric} sample reports the unreadable value {value:?}")),
    })
}

/// Read the `name="value"` pairs one sample declares between its braces.
///
/// The comma is read as a separator wherever it appears, so a label value
/// carrying one is split into two pairs and fails here rather than parsing. That
/// restriction is stated rather than lifted: the counters this reader exists for
/// carry the closed rejection vocabulary — a category name and a status, both
/// static text with no comma in any of them — so a quote-aware scanner would add
/// a branch to test support that no scrape can reach and no test can drive. A
/// reader that needs one is reading a different metric and should say so here
/// first.
fn parse_labels(labels: &str) -> Box<[(Box<str>, Box<str>)]> {
    labels
        .split(',')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or_else(|| {
                panic!("a scraped label is not a name=value pair: {pair:?} in {labels:?}")
            });
            (Box::from(name), Box::from(value.trim_matches('"')))
        })
        .collect()
}

/// The value one scrape reports for every sample the predicate selects, and
/// zero for none.
///
/// Absence is zero because a counter that has never been incremented is not
/// printed at all: a reader that failed on absence could not state a delta
/// against a label set the fixture is about to create.
fn value_where(samples: &[Sample], matching: impl Fn(&Sample) -> bool) -> u64 {
    samples
        .iter()
        .filter(|sample| matching(sample))
        .map(Sample::value)
        .sum()
}

/// How far the samples one predicate selects moved between two scrapes.
///
/// A counter only rises, so a fall is a different recorder answering — a
/// reading this cannot be a delta of, and it fails rather than wrapping. Stated
/// once for every way of selecting samples, because the monotonicity rule
/// belongs to the counter and not to the labels a caller picked it out by.
/// `subject` names that selection in the failure.
fn delta_where(
    before: &[Sample],
    after: &[Sample],
    subject: &str,
    matching: impl Fn(&Sample) -> bool,
) -> u64 {
    let (start, end) = (
        value_where(before, &matching),
        value_where(after, &matching),
    );
    assert!(
        end >= start,
        "the counter for {subject} fell from {start} to {end}"
    );
    end - start
}

/// How far one counter moved between two scrapes.
pub fn delta(before: &[Sample], after: &[Sample], labels: &[(&str, &str)]) -> u64 {
    delta_where(before, after, &format!("{labels:?}"), |sample| {
        sample.matches(labels)
    })
}

/// How far every sample carrying `name = value` moved between two scrapes.
pub fn delta_by(before: &[Sample], after: &[Sample], name: &str, value: &str) -> u64 {
    delta_where(before, after, &format!("{name}={value}"), |sample| {
        sample.label(name) == Some(value)
    })
}
