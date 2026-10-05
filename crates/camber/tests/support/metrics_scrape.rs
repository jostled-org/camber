//! One Prometheus scrape, read as the samples it declares.
//!
//! Three readers assert on the same rendered text — the component
//! observability root, the core-acceptance operator journeys, and the
//! integration terminal rows — so the text format is parsed once here. A
//! second copy is a second thing that can disagree about what a label set
//! means.

type Labels = Box<[(Box<str>, Box<str>)]>;

/// One sample line: the metric it reports under, the labels it carries, and
/// the value it reports.
///
/// `Debug` is what every rule this sample can break reports it under: a
/// vocabulary check that failed without naming the sample tells an operator a
/// rule was broken and leaves them to find which line broke it.
#[derive(Debug)]
pub struct Sample {
    name: Box<str>,
    labels: Labels,
    value: f64,
}

/// The largest whole number an `f64` holds exactly: a counter past it could
/// no longer be told apart from its neighbour.
const MAX_EXACT_COUNT: f64 = 9_007_199_254_740_992.0;

impl Sample {
    /// The metric name this sample reports under.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The value this sample reports, read as a counter.
    ///
    /// Private: [`value_where`] is the only reader, and every delta this module
    /// hands out is a sum over a selection. A caller holding one sample's value
    /// would be reading a counter one label set at a time, which is the reading
    /// [`delta_by`] exists so that nobody has to do.
    ///
    /// A counter is whole: a scrape that reports a fractional or negative
    /// value for one is a scrape this reader has no meaning for, and it fails
    /// rather than rounding one in. The cast below reads only a value that
    /// check admitted.
    fn count(&self) -> u64 {
        let value = self.value;
        assert!(
            (0.0..=MAX_EXACT_COUNT).contains(&value) && value.fract() == 0.0,
            "a {} sample reports {value}, which no counter can hold",
            self.name
        );
        value as u64
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
///
/// Two questions, kept apart because conflating them is what lets a sample
/// vanish. Whether the line is this metric's at all is decided first, by its
/// whole name: a line for another metric that merely begins with the same
/// name — the histogram's `_sum` beside its counter — names another metric, so
/// it is skipped. Once the line IS this metric's, it must read; a line that
/// will not parse is a scrape this reader has no meaning for, and it fails
/// rather than dropping a sample the caller asked for and reporting zero.
fn samples(scrape: &str, metric: &str) -> Box<[Sample]> {
    scrape
        .lines()
        .filter(|line| sample_name(line) == Some(metric))
        .map(|line| {
            parse_sample(line)
                .unwrap_or_else(|| panic!("a {metric} sample is unreadable: {line:?}"))
        })
        .collect()
}

/// The metric name a sample line opens with: everything before its label set
/// or its value.
fn sample_name(line: &str) -> Option<&str> {
    line.find(|character: char| character == '{' || character.is_whitespace())
        .map(|end| &line[..end])
}

/// Read one `name{label="value",...} value` line, or `None` when it is no
/// sample line.
///
/// A timestamp after the value is allowed and ignored.
pub fn parse_sample(line: &str) -> Option<Sample> {
    let name = sample_name(line)?;
    let rest = &line[name.len()..];
    let (labels, rest) = match rest.strip_prefix('{') {
        Some(block) => parse_labels(block)?,
        None => (Box::default(), rest),
    };
    let value = rest
        .strip_prefix(char::is_whitespace)?
        .split_whitespace()
        .next()?;
    Some(Sample {
        name: name.into(),
        labels,
        value: value.parse().ok()?,
    })
}

/// Read the quoted `name="value"` pairs one label set declares, through its
/// closing brace, and return them with the text after it.
///
/// Quote-aware: a comma, a brace, or an escaped quote inside a value belongs
/// to the value. The value is kept as the scrape escaped it.
fn parse_labels(mut rest: &str) -> Option<(Labels, &str)> {
    let mut labels = Vec::new();
    loop {
        if let Some(after) = rest.strip_prefix('}') {
            return Some((labels.into_boxed_slice(), after));
        }
        let (name, quoted) = rest.split_once("=\"")?;
        let named = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !named {
            return None;
        }
        let mut escaped = false;
        let end = quoted.char_indices().find_map(|(at, character)| {
            let closes = character == '"' && !escaped;
            escaped = character == '\\' && !escaped;
            closes.then_some(at)
        })?;
        labels.push((name.into(), quoted[..end].into()));
        let after = &quoted[end + 1..];
        rest = after.strip_prefix(',').unwrap_or(after);
    }
}

/// The total one scrape reports over every sample the predicate selects, and
/// zero for none.
///
/// The reading for a family whose samples are not all counters, such as a
/// duration's `_sum`: a sum over a selection, never one sample's value.
pub fn total_where(samples: &[Sample], matching: impl Fn(&Sample) -> bool) -> f64 {
    selected(samples, matching).map(|sample| sample.value).sum()
}

/// The samples the predicate selects, for every sum this module takes.
fn selected(
    samples: &[Sample],
    matching: impl Fn(&Sample) -> bool,
) -> impl Iterator<Item = &Sample> {
    samples.iter().filter(move |sample| matching(sample))
}

/// The value one scrape reports for every sample the predicate selects, and
/// zero for none.
///
/// Absence is zero because a counter that has never been incremented is not
/// printed at all: a reader that failed on absence could not state a delta
/// against a label set the fixture is about to create.
fn value_where(samples: &[Sample], matching: impl Fn(&Sample) -> bool) -> u64 {
    selected(samples, matching).map(Sample::count).sum()
}

/// How far the samples one predicate selects moved between two scrapes.
///
/// A counter only rises, so a fall is a different recorder answering — a
/// reading this cannot be a delta of, and it fails rather than wrapping. Stated
/// once for every way of selecting samples, because the monotonicity rule
/// belongs to the counter and not to the labels a caller picked it out by.
/// `subject` names that selection in the failure, and is rendered only there.
fn delta_where(
    before: &[Sample],
    after: &[Sample],
    subject: std::fmt::Arguments<'_>,
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
    delta_where(before, after, format_args!("{labels:?}"), |sample| {
        sample.matches(labels)
    })
}

/// How far every sample carrying `name = value` moved between two scrapes.
pub fn delta_by(before: &[Sample], after: &[Sample], name: &str, value: &str) -> u64 {
    delta_where(before, after, format_args!("{name}={value}"), |sample| {
        sample.label(name) == Some(value)
    })
}
