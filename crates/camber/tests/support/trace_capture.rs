//! One capturing subscriber per test binary, shared by every test that reads
//! what production recorded.
//!
//! A process accepts one global subscriber, and the events these tests need are
//! emitted from runtime worker threads, where a thread-local subscriber would
//! never see them. So every capture goes through this one bus: it is installed
//! on first use and fans each event out to the subscriptions that named it.

use std::fmt::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Once};

/// How many bytes of transcript one capture retains.
///
/// The same budget the child-process capture in `process.rs` holds its pipes
/// to. A capture is fed by whatever production writes while the handle is
/// alive, so a subject that logs in a loop would otherwise turn a bounded
/// assertion failure into an exhausted test host.
const CAPTURE_LIMIT: usize = 64 * 1024;

/// One test's captured transcript, held to [`CAPTURE_LIMIT`].
///
/// The overflow is recorded, not swallowed: a transcript cut short still
/// answers "was this recorded?" for what it kept, and answers nothing at all
/// for what it dropped. Keeping the flag beside the events is what lets the
/// readers tell those two apart.
#[derive(Default)]
struct Transcript {
    events: Vec<Box<str>>,
    bytes: usize,
    truncated: bool,
}

impl Transcript {
    /// Record `text` while the budget covers it, and mark the transcript short
    /// once it does not.
    fn push(&mut self, text: &str) {
        match self.bytes.checked_add(text.len()) {
            Some(total) if total <= CAPTURE_LIMIT => {
                self.bytes = total;
                self.events.push(text.into());
            }
            _ => self.truncated = true,
        }
    }
}

/// One test's standing interest in the events naming it.
struct Subscription {
    needle: Box<str>,
    events: Arc<Mutex<Transcript>>,
}

/// The live subscriptions the bus fans out to.
///
/// Initialised on its own, before anything installs the subscriber that reads
/// it: an event emitted on the installing thread reaches [`CaptureBus::on_event`]
/// while installation is still in progress, so this cell must already be
/// readable there rather than half-built by the same call.
static SUBSCRIPTIONS: LazyLock<Mutex<Vec<Subscription>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Installs the global subscriber exactly once, separately from the
/// subscriptions it feeds.
static INSTALL: Once = Once::new();

/// Whether the one installation attempt was refused.
///
/// Recorded rather than panicked on: a panic inside [`INSTALL`] would poison
/// it, and every later caller would then fail on the poisoning instead of on
/// the reason. The verdict is read by each caller, so all of them report the
/// same authored diagnosis.
static INSTALL_REFUSED: AtomicBool = AtomicBool::new(false);

/// The subscriptions the bus fans out to.
fn subscriptions() -> &'static Mutex<Vec<Subscription>> {
    &SUBSCRIPTIONS
}

/// Make the capture bus this binary's global subscriber.
fn install_bus() {
    INSTALL.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;

        let subscriber = tracing_subscriber::registry().with(CaptureBus);
        let installed = camber::tracing::subscriber::set_global_default(subscriber);
        INSTALL_REFUSED.store(installed.is_err(), Ordering::Release);
    });
    // A test that installs its own subscriber would leave every capture here
    // empty, which reads as "production recorded nothing".
    assert!(
        !INSTALL_REFUSED.load(Ordering::Acquire),
        "another subscriber already owns this test binary, so no capture can record anything"
    );
}

/// The global layer: formats an event's fields once and hands the text to
/// every subscription that named part of it.
struct CaptureBus;

impl<S> tracing_subscriber::Layer<S> for CaptureBus
where
    S: camber::tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &camber::tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let subscriptions = subscriptions()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Nothing is capturing, so nothing is formatted: the bus costs the
        // rest of the binary a lock and a length check.
        match subscriptions.is_empty() {
            true => {}
            false => fan_out(event, &subscriptions),
        }
    }
}

/// Record `event` into each subscription whose needle appears in its fields.
fn fan_out(event: &camber::tracing::Event<'_>, subscriptions: &[Subscription]) {
    let mut fields = FieldText(String::new());
    named(&mut fields, event.metadata().name());
    event.record(&mut fields);
    let text = fields.0;
    for subscription in subscriptions
        .iter()
        .filter(|subscription| text.contains(subscription.needle.as_ref()))
    {
        subscription
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(text.as_str());
    }
}

/// Open the transcript with `name=<event name>` when production named the event.
///
/// A `tracing` macro given no name generates `"event <file>:<line>"`, which is
/// the call site rather than a claim about what the event is. Recording that
/// would put a source path in front of every captured event and let a needle
/// match on where the code lives. A name production chose carries no space, so
/// the two are told apart by exactly the text the macro would have generated.
fn opening_name(name: &'static str) -> Option<&'static str> {
    match name.starts_with("event ") {
        true => None,
        false => Some(name),
    }
}

/// Write the opening `name=` field, for an event that has a name of its own.
fn named(fields: &mut FieldText, name: &'static str) {
    if let Some(name) = opening_name(name) {
        fields.0.push_str("name=");
        fields.0.push_str(name);
    }
}

/// An event's fields as `name=value` text.
///
/// String values are recorded unquoted so a needle can name a path exactly as
/// production wrote it.
struct FieldText(String);

impl FieldText {
    /// Open one field — the separator and `name=` — and hand back the
    /// accumulator its value is written straight into.
    ///
    /// The prefix is stated once, and the value never lands in a temporary
    /// first. Every field of every event passes through here while any capture
    /// is live, so a `String` per field would be an allocation per field.
    fn open(&mut self, field: &camber::tracing::field::Field) -> &mut String {
        match self.0.is_empty() {
            true => {}
            false => self.0.push(' '),
        }
        self.0.push_str(field.name());
        self.0.push('=');
        &mut self.0
    }
}

impl camber::tracing::field::Visit for FieldText {
    fn record_debug(&mut self, field: &camber::tracing::field::Field, value: &dyn std::fmt::Debug) {
        write!(self.open(field), "{value:?}").expect("a write into a String cannot fail");
    }

    /// Recorded unquoted, unlike every other type.
    ///
    /// The trait's own default sends a string through `record_debug`, which
    /// quotes and escapes it — and a needle naming a path exactly as production
    /// wrote it would then never match. The integer defaults need no such
    /// override: they forward to `record_debug` too, and `{:?}` on an integer is
    /// what `{}` would have written.
    fn record_str(&mut self, field: &camber::tracing::field::Field, value: &str) {
        self.open(field).push_str(value);
    }
}

/// The events one test asked for, captured while this handle is alive.
pub struct TraceCapture {
    events: Arc<Mutex<Transcript>>,
}

impl TraceCapture {
    /// Every event captured so far, one entry per event.
    ///
    /// Fails on a transcript the budget cut short, because the caller is asking
    /// for the whole record and this is no longer one: a search over a truncated
    /// transcript that finds nothing reads as "production recorded nothing".
    pub fn events(&self) -> Box<[Box<str>]> {
        let transcript = self.lock();
        assert!(
            !transcript.truncated,
            "the captured transcript exceeded its {CAPTURE_LIMIT}-byte budget, so it is not the whole record"
        );
        transcript.events.iter().cloned().collect()
    }

    /// How many events were captured.
    ///
    /// For the callers that want only the count: cloning the whole transcript to
    /// take its length copies every event to answer a question the length alone
    /// answers.
    pub fn len(&self) -> usize {
        self.lock().events.len()
    }

    /// Whether nothing was captured.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the budget cut the transcript short.
    pub fn truncated(&self) -> bool {
        self.lock().truncated
    }

    /// Whether one captured event carries all of `fields`.
    ///
    /// One event, not the union of several: a record is only a record if the
    /// producer, the request it belonged to, and the reason arrived together.
    ///
    /// A miss on a truncated transcript fails rather than answering `false`: the
    /// event asked about may be one of the dropped ones, and reporting its
    /// absence would be reporting something this capture does not know.
    pub fn recorded(&self, fields: &[&str]) -> bool {
        let transcript = self.lock();
        let found = transcript
            .events
            .iter()
            .any(|event| fields.iter().all(|field| event.contains(field)));
        assert!(
            found || !transcript.truncated,
            "the captured transcript exceeded its {CAPTURE_LIMIT}-byte budget, so it cannot report {fields:?} as absent"
        );
        found
    }

    /// The transcript, with a poisoned lock read through rather than reported.
    ///
    /// A capture is read while other tests are running, and one of them
    /// panicking mid-push poisons this lock without corrupting the events
    /// already in it.
    fn lock(&self) -> std::sync::MutexGuard<'_, Transcript> {
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

impl Drop for TraceCapture {
    fn drop(&mut self) {
        let mut subscriptions = subscriptions()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        subscriptions.retain(|subscription| !Arc::ptr_eq(&subscription.events, &self.events));
    }
}

/// The captured events carrying one fixed sentence.
fn events_saying<'a>(events: &'a [Box<str>], sentence: &str) -> Box<[&'a str]> {
    events
        .iter()
        .map(Box::as_ref)
        .filter(|event| event.contains(sentence))
        .collect()
}

/// The one captured event carrying `sentence`, or a failure naming what was there.
///
/// One event, not the first of several: a record that a condition happened once
/// is only a record if nothing else recorded the same condition.
pub fn only_event<'a>(events: &'a [Box<str>], sentence: &str, label: &str) -> &'a str {
    let matching = events_saying(events, sentence);
    assert_eq!(
        matching.len(),
        1,
        "{label}: exactly one {sentence:?} event was expected, but {} of the {} captured carry it: {matching:?}",
        matching.len(),
        events.len()
    );
    matching[0]
}

/// Assert one event carries every declared field.
///
/// A row with no declared fields is refused rather than passed: this is called
/// table-driven from two roots, and a row that named nothing would assert
/// nothing while reporting the same success as a row that named everything.
pub fn assert_fields(event: &str, fields: &[&str], label: &str) {
    assert!(
        !fields.is_empty(),
        "{label}: no fields were declared, so this event proves nothing: {event}"
    );
    for field in fields {
        assert!(
            event.contains(field),
            "{label}: the event does not carry {field:?}: {event}"
        );
    }
}

/// Assert one event records `name` with exactly `value`.
///
/// Field-aware, unlike [`assert_fields`]: `status=500` is a substring of
/// `default_status=500`, so a contains-check can report a status the event
/// never recorded. Reads the boundary the capture writes — fields are
/// `name=value`, separated by one space — so it answers for the field asked
/// about and no other.
///
/// For fields whose value cannot contain a space. The message and the source
/// chain both can, and neither is a value a row states exactly.
pub fn assert_field_value(event: &str, name: &str, value: &str, label: &str) {
    let recorded = field_value(event, name)
        .unwrap_or_else(|| panic!("{label}: the event records no {name} field: {event}"));
    assert_eq!(
        recorded, value,
        "{label}: the event records {name}={recorded}, not {name}={value}: {event}"
    );
}

/// The value one event recorded under `name`, read at a field boundary.
///
/// Public for the callers that correlate two events on a value production
/// chose — a request identifier no wire response carried, so no row can state
/// it as a literal.
pub fn field_value<'a>(event: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("{name}=");
    let at = field_starts(event, needle.as_str()).next()?;
    let rest = &event[at + needle.len()..];
    Some(rest.split_once(' ').map_or(rest, |(value, _)| value))
}

/// How many times `event` opens a field called `name`.
///
/// A count rather than a first hit, for the rows whose claim is that a record
/// carries one of something: an event that named a field twice and an event
/// that named it once read the same to [`field_value`], which takes the first.
pub fn field_occurrences(event: &str, name: &str) -> usize {
    field_starts(event, &format!("{name}=")).count()
}

/// Where `event` opens a field spelled `needle`, at a field boundary.
///
/// The boundary is the whole of it. `name=` also appears inside
/// `request_name=`, so a scan that matched anywhere would read a longer field's
/// value as a shorter field's. A field starts the record or follows a space,
/// and that rule is stated once here for the reader and the counter alike.
fn field_starts<'a>(event: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    event
        .match_indices(needle)
        .filter(|(at, _)| *at == 0 || event.as_bytes()[at - 1] == b' ')
        .map(|(at, _)| at)
}

/// Assert the fixed sentence is the whole message, with nothing spliced into it.
///
/// The sentence is read where the capture wrote it, and what follows it must be
/// the end of the record or the next field. A separator alone is not enough:
/// `"request rejected {cause}"` interpolates a cause that begins with a space,
/// so a tail that only had to start with one would pass the very check this
/// exists to fail. What follows is required to look like a field instead.
pub fn assert_message_is_fixed(event: &str, sentence: &str, label: &str) {
    let at = event
        .find(sentence)
        .unwrap_or_else(|| panic!("{label}: the event does not carry {sentence:?}: {event}"));
    let tail = &event[at + sentence.len()..];
    assert!(
        tail.is_empty() || opens_a_field(tail),
        "{label}: the fixed message carries interpolated text: {event}"
    );
}

/// Whether `tail` continues into another field rather than into spliced text.
///
/// A field is `name=value` behind one space, so the tail has to open with that
/// separator and its first token has to carry a `=` with a name in front of it.
/// Interpolated text carries the second of those at best, whichever character it
/// starts with.
fn opens_a_field(tail: &str) -> bool {
    tail.starts_with(' ')
        && tail
            .split_whitespace()
            .next()
            .and_then(|token| token.split_once('='))
            .is_some_and(|(name, _)| !name.is_empty())
}

/// How many live captures asked for exactly `needle`.
///
/// Scoped to one needle rather than reported as a total, so a contract test can
/// prove its own subscription was added and then removed while the rest of the
/// binary captures whatever it likes on other needles.
pub fn captures_for(needle: &str) -> usize {
    subscriptions()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .filter(|subscription| subscription.needle.as_ref() == needle)
        .count()
}

/// Capture every event whose recorded fields contain `needle`.
///
/// Capturing starts here and ends when the returned handle drops, so a test
/// reads only what it asked for even while the rest of the binary runs.
pub fn capture_events(needle: &str) -> TraceCapture {
    install_bus();
    let events = Arc::new(Mutex::new(Transcript::default()));
    subscriptions()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(Subscription {
            needle: needle.into(),
            events: Arc::clone(&events),
        });
    TraceCapture { events }
}
