//! 10.T1 — invariant 15: the completion dimensions are orthogonal and set once.
//!
//! Every fact a finished operation carries used to be folded into one strongest
//! terminal, and a fold has to erase to answer. An application response cut short
//! by a departing peer was reported as "disconnect" with no producer left in it;
//! a payload past its route maximum was reported as "route_body_limit" with no
//! statement that the refusal it produced actually reached the peer.
//!
//! This root drives three admitted operations through the completion observation
//! production already publishes and reads what each one was recorded as. The
//! claim is not that any single dimension is right — 10.T2 owns that per
//! producer — but that the three rows differ in exactly the dimensions their
//! journeys differ in, and agree in the rest. A fold cannot satisfy both halves
//! at once.

use crate::common;

use camber::http::{Request, Response, Router, StreamResponse};
use camber::runtime;
use std::time::Duration;

/// The counter one completed operation is recorded under.
const COMPLETION_METRIC: &str = "http_requests_total";

/// The sentence one completed operation is recorded under.
const COMPLETION_EVENT: &str = "message=request completed";

/// How long any row's exchange, read, or rendezvous may take.
const ORTHOGONAL_BOUND: Duration = Duration::from_secs(10);

/// The payload maximum this root's routes are admitted under.
const ORTHOGONAL_MAX_BODY: usize = 16;

/// The routes the three rows are driven on.
///
/// Each names its own dispatch class, because a record is selected by its whole
/// label set and two rows sharing a class would read one counter. The methods are
/// the other half of that: this root's counters are process-global, so a row
/// answered `GET`/`200` on ordinary HTTP would count every sibling case in the
/// binary that serves one too.
const PRODUCED_PATH: &str = "/orthogonal/produced";
const INTERRUPTED_PATH: &str = "/orthogonal/interrupted";
const REFUSED_PATH: &str = "/orthogonal/refused";

/// The name every absent completion dimension is published under.
const ABSENT: &str = "none";

/// Every dimension one completion record publishes, beside its identity.
///
/// Named as one closed list because the claim below is about the whole record:
/// a check that read the dimensions a row varies and ignored the rest could not
/// tell a record that left them absent from one that folded them away.
const DIMENSIONS: [&str; 7] = [
    "status",
    "origin",
    "rejection",
    "delivery",
    "connection_end",
    "boundary",
    "shutdown",
];

/// What one row's record must carry, dimension by dimension.
///
/// Every field is stated, including the absences, so a row says what it is about
/// and what it deliberately is not.
struct Facts {
    label: &'static str,
    method: &'static str,
    /// The raw path this row drives, which selects its own record.
    ///
    /// The dispatch class cannot: this fixture scrapes its own `/metrics`, which
    /// is an admitted ordinary-HTTP operation of its own and is recorded beside
    /// every row that shares that class.
    path: &'static str,
    protocol: &'static str,
    status: &'static str,
    origin: &'static str,
    rejection: &'static str,
    delivery: &'static str,
    connection_end: &'static str,
    boundary: &'static str,
    shutdown: &'static str,
}

impl Facts {
    /// The seven dimensions, paired with the names production publishes them
    /// under.
    ///
    /// Built from the one declared list rather than transcribed beside it, so a
    /// dimension this file names and a dimension it checks are one list.
    fn stated(&self) -> [(&'static str, &'static str); DIMENSIONS.len()] {
        std::array::from_fn(|at| (DIMENSIONS[at], self.value_of(DIMENSIONS[at])))
    }

    /// This row's value for the dimension production publishes as `name`.
    ///
    /// The one place a declared dimension meets the field that answers it. A
    /// name in [`DIMENSIONS`] this row cannot answer fails every row here,
    /// rather than leaving a renamed dimension checked under its old spelling.
    fn value_of(&self, name: &str) -> &'static str {
        match name {
            "status" => self.status,
            "origin" => self.origin,
            "rejection" => self.rejection,
            "delivery" => self.delivery,
            "connection_end" => self.connection_end,
            "boundary" => self.boundary,
            "shutdown" => self.shutdown,
            other => panic!("{other} is declared a completion dimension and answered by no field"),
        }
    }

    /// The whole label set this row's counter sample carries.
    fn labels(&self) -> Box<[(&'static str, &'static str)]> {
        [("method", self.method), ("protocol", self.protocol)]
            .into_iter()
            .chain(self.stated())
            .collect()
    }
}

/// The three rows, and the exact facts each one must be recorded under.
///
/// Read them down a column rather than across a row: `origin` says application
/// twice and framework once, `delivery` says produced twice and interrupted
/// once, and `connection_end` says absent twice and peer-disconnected once. No
/// two columns agree, which is what makes each of them a dimension of its own
/// rather than a rendering of one underlying answer.
const ROWS: [Facts; 3] = [
    Facts {
        label: "produced",
        method: "PATCH",
        path: PRODUCED_PATH,
        protocol: "ordinary_http",
        status: "200",
        origin: "application",
        rejection: ABSENT,
        delivery: "produced",
        connection_end: ABSENT,
        boundary: ABSENT,
        shutdown: ABSENT,
    },
    Facts {
        label: "interrupted",
        method: "GET",
        path: INTERRUPTED_PATH,
        protocol: "streaming_http",
        status: "200",
        // The whole point of the row: the application still produced this head.
        // A strongest terminal reported the departing peer and lost the producer.
        origin: "application",
        rejection: ABSENT,
        delivery: "interrupted",
        connection_end: "peer-disconnected",
        boundary: ABSENT,
        shutdown: ABSENT,
    },
    Facts {
        label: "refused",
        method: "PUT",
        path: REFUSED_PATH,
        protocol: "ordinary_http",
        status: "413",
        // The mirror image: a crossed bound used to be the whole answer, and the
        // refusal it produced reached the peer in full all the same.
        origin: "framework",
        rejection: "body_limit",
        delivery: "produced",
        connection_end: ABSENT,
        boundary: "request_body",
        shutdown: ABSENT,
    },
];

/// The held producer's release, kept alive for as long as the fixture is.
///
/// Never sent on: the interrupted row's whole claim is a body still owed when
/// its peer left. Held all the same, because dropping it closes the channel and
/// lets the spawned producer end with the runtime rather than outliving it.
type Release = tokio::sync::mpsc::UnboundedSender<()>;

/// The routes the three rows are answered by.
///
/// The interrupted row's body is held rather than raced: its head is on the wire
/// and its body is still owed at the moment the peer leaves, which is the only
/// state in which an interrupted delivery under a committed application origin
/// can be observed at all.
fn served_routes() -> (Router, Release) {
    let (release, releases) = tokio::sync::mpsc::unbounded_channel::<()>();
    let releases = std::sync::Arc::new(tokio::sync::Mutex::new(releases));
    let mut routes = Router::new();
    routes.patch(PRODUCED_PATH, |_req: &Request| async {
        Response::text(200, "produced")
    });
    routes.get_stream(INTERRUPTED_PATH, move |_req: &Request| {
        let releases = std::sync::Arc::clone(&releases);
        Box::pin(async move {
            let (response, sender) = StreamResponse::new(200);
            tokio::spawn(async move {
                if let Some(()) = releases.lock().await.recv().await {
                    drop(sender.send("released").await);
                }
            });
            response.with_header("Content-Type", "text/x-held")
        })
    });
    routes.put(REFUSED_PATH, |_req: &Request| async {
        Response::text(200, "admitted")
    });
    (routes.max_request_body(ORTHOGONAL_MAX_BODY), release)
}

/// What one scrape reports about completed operations.
fn scraped(addr: std::net::SocketAddr) -> Box<[common::Sample]> {
    let scrape = common::send(addr, "GET", "/metrics", &[], b"");
    common::scraped_samples(&scrape, COMPLETION_METRIC)
}

/// Drive the produced row: a whole buffered answer the peer reads.
fn drive_produced(addr: std::net::SocketAddr) {
    let answered = common::send(addr, "PATCH", PRODUCED_PATH, &[], b"");
    assert_eq!(answered.status, 200, "produced: wire status");
}

/// Drive the interrupted row: a committed head whose body the peer abandons.
fn drive_interrupted(addr: std::net::SocketAddr) {
    let label = "interrupted";
    let mut peer = common::connect(addr)
        .unwrap_or_else(|error| panic!("{label}: the peer could not connect: {error}"));
    common::write_request(&mut peer, "GET", INTERRUPTED_PATH, &[], b"")
        .unwrap_or_else(|error| panic!("{label}: the peer could not send: {error}"));
    let head = common::read_head(&mut peer, ORTHOGONAL_BOUND)
        .unwrap_or_else(|error| panic!("{label}: no head arrived: {error}"));
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{label}: the application head never committed: {head}",
    );
    // The head is on the wire and the body is still owed. Leaving now is the
    // only way an interrupted delivery under a committed origin can be observed.
    drop(peer);
}

/// Drive the refused row: a payload past the maximum its route admits.
fn drive_refused(addr: std::net::SocketAddr) {
    let answered = common::send(
        addr,
        "PUT",
        REFUSED_PATH,
        &[],
        &[b'x'; ORTHOGONAL_MAX_BODY * 4],
    );
    assert_eq!(answered.status, 413, "refused: wire status");
}

/// Assert one row's counter sample carries exactly the facts it declared.
fn assert_counted_once(before: &[common::Sample], after: &[common::Sample], facts: &Facts) {
    assert_eq!(
        common::delta(before, after, &facts.labels()),
        1,
        "{}: exactly one record was expected under {:?}",
        facts.label,
        facts.labels(),
    );
}

/// Assert one row's event states every dimension once, and states it correctly.
///
/// The set-once half is the count: a dimension a second writer could reach would
/// show up twice in the record it is written into, and a dimension a fold
/// rewrote would show up under a value no owner of this row ever named.
fn assert_stated_once(event: &str, facts: &Facts) {
    for (name, value) in facts.stated() {
        let stated = common::field_occurrences(event, name);
        assert_eq!(
            stated, 1,
            "{}: the record states {name} {stated} times: {event}",
            facts.label,
        );
        assert_eq!(
            common::field_value(event, name),
            Some(value),
            "{}: the record names the wrong {name}: {event}",
            facts.label,
        );
    }
}

/// 10.T1 — invariant 15
///
/// Response origin, delivery outcome, connection end, crossed boundary, and
/// shutdown observation are orthogonal completion facts. Three admitted
/// operations that differ in one dimension each are recorded differing in that
/// dimension alone: the interrupted body keeps the application origin a fold
/// erased, and the refused payload keeps the produced delivery a fold erased.
/// Each dimension is stated exactly once per record.
#[test]
fn completion_facts_keep_dimensions_orthogonal_and_set_once() {
    common::test_runtime()
        .with_metrics()
        .with_tracing()
        .run(|| {
            let (routes, release) = served_routes();
            let addr = common::spawn_server(routes);

            for facts in &ROWS {
                let capture = common::capture_events(COMPLETION_EVENT);
                let before = scraped(addr);
                match facts.path {
                    PRODUCED_PATH => drive_produced(addr),
                    INTERRUPTED_PATH => drive_interrupted(addr),
                    _ => drive_refused(addr),
                }
                let settled = common::poll_until(ORTHOGONAL_BOUND, || {
                    common::delta(&before, &scraped(addr), &facts.labels()) >= 1
                });
                assert!(
                    settled,
                    "{}: no record appeared for this operation's terminal",
                    facts.label,
                );
                assert_counted_once(&before, &scraped(addr), facts);

                let events = capture.events();
                let recorded = events
                    .iter()
                    .find(|event| common::field_value(event, "path") == Some(facts.path))
                    .unwrap_or_else(|| {
                        panic!(
                            "{}: no completion event was recorded: {events:?}",
                            facts.label
                        )
                    });
                assert_stated_once(recorded, facts);
            }

            // Released after every row, so the held producer's own task ends
            // inside this runtime rather than outliving the fixture.
            drop(release);
            runtime::request_shutdown();
        })
        .expect("the orthogonal completion runtime ran to completion");
}
