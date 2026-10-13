//! 1.T2: the closed outbound failure taxonomy, its safe renderings, and the
//! one refusal every category becomes when a handler returns it.
//!
//! The category rows are values. The projection rows feed a real Reqwest
//! failure to the production projection, reached through the doc-hidden
//! `__private` surface, so the classification proved here is the one a failed
//! request is given. The DNS row answers its own lookup, so no public resolver
//! is asked anything. This proves the categories, not real DNS routing. The
//! connect-timeout row asks a resolver that never answers, so the failure is a
//! timeout and a connect failure at once. The redirect row follows a local
//! peer that answers every request with a redirect to itself, until Reqwest's
//! own hop ceiling refuses the next hop. The connect, transport, and TLS rows
//! take the failures the replay matrix in `retry_delays` builds, each from a
//! local peer.
//!
//! 7.T3: the outcome a completion event records for each failure, through the
//! production projection `client_terminal_outcome(&RuntimeError) -> &'static
//! str`, reached the same way. The category rows and the raw Reqwest rows are
//! fed to it, so `configuration` and `dns` are proved without a peer. Three
//! public requests that end before any send are fed to it too. This proves the
//! vocabulary is closed and complete. It does not prove that an event is
//! emitted: the observability root owns that.
//!
//! Egress 1.T2: the destination-policy categories join every table above. The
//! refusals a real policy returns are read for the text they were built from.
//! The replay decision is the production one, reached through `__private` as
//! `client_retryable_failure(ClientError, &Method, RetryPolicy) -> bool`. The
//! served refusal is entered at the handler stage and at the middleware stage.
//!
//! Clause → row → the mutation that turns the row red:
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | Each reason has its own safe name | `assert_nested_categories_are_named`, `assert_safe_formatting` | erase one Display or category arm |
//! | A refusal records `egress` | `assert_categories_project_closed_outcomes`, run by 7.T3 | erase the outcome arm |
//! | No refusal is replayed | `assert_egress_refusals_never_replay` | make Address replayable |
//! | No refusal repeats its inputs | `assert_egress_refusals_keep_no_text` | expose exception text |
//! | A middleware failure is this service's | `assert_internal_service_refusals`, middleware stage | remove the Client classification arm |
//!
//! Streaming 4.T3: `ClientInput::StreamBudget` and
//! `ClientTimeout::ResponseIdle` join every table above. Both are values here.
//! No row starts a stream, so this proves the vocabulary and its consumers,
//! not that a stream returns either one.
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | Each addition has its own safe name | `assert_nested_categories_are_named`, `assert_safe_formatting` | share a label with `ResponseLimit` or `Transport` |
//! | Each addition records its outcome | `assert_categories_project_closed_outcomes`, run by 7.T3 | erase the outcome arm |
//! | Neither addition is sent again | `assert_stream_categories_never_replay` | retry `ResponseIdle` |
//! | Neither is an inbound deadline refusal | `assert_internal_service_refusals`, both stages | fall through to a legacy `DeadlineBoundary` |

use crate::client_egress::{EXCEPTED, decide, excepting, ip};
use crate::error_chain::{assert_no_raw_source, assert_safe, assert_secret_free, renderings};
use crate::http::{UndialedListener, WIRE_TIMEOUT, read_head, request};
use crate::leaky_source::LeakySource;
use crate::retry_delays;
use crate::runtime_support::{spawn_server, test_runtime};

use camber::__private::client_error_projection;
use camber::__private::client_retryable_failure;
use camber::__private::client_terminal_outcome;
use camber::http::Client;
use camber::http::ClientDecode;
use camber::http::ClientEgress;
use camber::http::ClientError;
use camber::http::ClientInput;
use camber::http::ClientRedirect;
use camber::http::ClientRequest;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::EgressPolicy;
use camber::http::Next;
use camber::http::{Rejection, RejectionContext, RejectionKind, Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;
use std::collections::BTreeSet;
use std::fmt::{Debug, Display};
use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// The fixed text a peer and a mapper are given for an internal refusal.
const FIXED_MESSAGE: &str = "internal server error";

/// The status an internal refusal carries.
const INTERNAL_STATUS: u16 = 500;

/// The ceiling the response-limit row names.
const ROW_LIMIT: usize = 4096;

/// One category, built fresh for every reader.
struct Row {
    /// What the row is called in a failure message. Unique.
    label: &'static str,
    /// The name [`category_of`] must give the built value.
    category: &'static str,
    build: fn() -> ClientError,
}

const ROWS: [Row; 30] = [
    Row {
        label: "configuration",
        category: "configuration",
        build: || ClientError::Configuration,
    },
    Row {
        label: "invalid method",
        category: "invalid-request/method",
        build: || ClientError::InvalidRequest(ClientInput::Method),
    },
    Row {
        label: "invalid url",
        category: "invalid-request/url",
        build: || ClientError::InvalidRequest(ClientInput::Url),
    },
    Row {
        label: "invalid header",
        category: "invalid-request/header",
        build: || ClientError::InvalidRequest(ClientInput::Header),
    },
    Row {
        label: "reserved header",
        category: "invalid-request/reserved-header",
        build: || ClientError::InvalidRequest(ClientInput::ReservedHeader),
    },
    Row {
        label: "invalid body",
        category: "invalid-request/body",
        build: || ClientError::InvalidRequest(ClientInput::Body),
    },
    Row {
        label: "invalid response limit",
        category: "invalid-request/response-limit",
        build: || ClientError::InvalidRequest(ClientInput::ResponseLimit),
    },
    Row {
        label: "invalid egress policy",
        category: "invalid-request/egress-policy",
        build: || ClientError::InvalidRequest(ClientInput::EgressPolicy),
    },
    Row {
        label: "invalid stream budget",
        category: "invalid-request/stream-budget",
        build: || ClientError::InvalidRequest(ClientInput::StreamBudget),
    },
    Row {
        label: "encode",
        category: "encode",
        build: || ClientError::Encode,
    },
    Row {
        label: "dns",
        category: "dns",
        build: || ClientError::Dns,
    },
    Row {
        label: "connect",
        category: "connect",
        build: || ClientError::Connect,
    },
    Row {
        label: "tls",
        category: "tls",
        build: || ClientError::Tls,
    },
    Row {
        label: "transport",
        category: "transport",
        build: || ClientError::Transport,
    },
    Row {
        label: "operation timeout",
        category: "timeout/operation",
        build: || ClientError::Timeout(ClientTimeout::Operation),
    },
    Row {
        label: "attempt timeout",
        category: "timeout/attempt",
        build: || ClientError::Timeout(ClientTimeout::Attempt),
    },
    Row {
        label: "transport timeout",
        category: "timeout/transport",
        build: || ClientError::Timeout(ClientTimeout::Transport),
    },
    Row {
        label: "response idle timeout",
        category: "timeout/response-idle",
        build: || ClientError::Timeout(ClientTimeout::ResponseIdle),
    },
    Row {
        label: "invalid location",
        category: "redirect/invalid-location",
        build: || ClientError::Redirect(ClientRedirect::InvalidLocation),
    },
    Row {
        label: "origin changed",
        category: "redirect/origin-changed",
        build: || ClientError::Redirect(ClientRedirect::OriginChanged),
    },
    Row {
        label: "hop limit",
        category: "redirect/hop-limit",
        build: || ClientError::Redirect(ClientRedirect::HopLimit),
    },
    Row {
        label: "unsafe redirect method",
        category: "redirect/unsafe-method",
        build: || ClientError::Redirect(ClientRedirect::UnsafeMethod),
    },
    Row {
        label: "status 404",
        category: "status",
        build: || ClientError::Status(404),
    },
    Row {
        label: "status 503",
        category: "status",
        build: || ClientError::Status(503),
    },
    Row {
        label: "utf-8 decode",
        category: "decode/utf8",
        build: || ClientError::Decode(ClientDecode::Utf8),
    },
    Row {
        label: "json decode",
        category: "decode/json",
        build: || ClientError::Decode(ClientDecode::Json),
    },
    Row {
        label: "response too large",
        category: "response-too-large",
        build: || ClientError::ResponseTooLarge { limit: ROW_LIMIT },
    },
    Row {
        label: "egress scheme",
        category: "egress/scheme",
        build: || ClientError::Egress(ClientEgress::Scheme),
    },
    Row {
        label: "egress address",
        category: "egress/address",
        build: || ClientError::Egress(ClientEgress::Address),
    },
    Row {
        label: "egress resolution limit",
        category: "egress/resolution-limit",
        build: || ClientError::Egress(ClientEgress::ResolutionLimit),
    },
];

/// A table with no rows would build no category and still report success.
const _: () = assert!(!ROWS.is_empty());

/// Declare [`CATEGORIES`] and [`category_of`] from one list of
/// `pattern => "name"` pairs, so neither can name a category the other lacks.
macro_rules! categories {
    ($($pattern:pat => $name:literal,)+) => {
        /// Every category the specification declares, by the name
        /// [`category_of`] gives it. A name two patterns share appears once
        /// for each. `status` has one pattern and two rows in [`ROWS`].
        const CATEGORIES: [&str; [$($name),+].len()] = [$($name),+];

        /// The category one value belongs to.
        ///
        /// The match has no wildcard arm at either level. A variant added to
        /// any of the six enums fails to compile until it is listed, and
        /// [`assert_every_category_has_a_row`] then fails until it is given a
        /// row.
        fn category_of(error: &ClientError) -> &'static str {
            match error {
                $($pattern => $name,)+
            }
        }
    };
}

categories! {
    ClientError::Configuration => "configuration",
    ClientError::InvalidRequest(ClientInput::Method) => "invalid-request/method",
    ClientError::InvalidRequest(ClientInput::Url) => "invalid-request/url",
    ClientError::InvalidRequest(ClientInput::Header) => "invalid-request/header",
    ClientError::InvalidRequest(ClientInput::ReservedHeader) => "invalid-request/reserved-header",
    ClientError::InvalidRequest(ClientInput::Body) => "invalid-request/body",
    ClientError::InvalidRequest(ClientInput::ResponseLimit) => "invalid-request/response-limit",
    ClientError::InvalidRequest(ClientInput::EgressPolicy) => "invalid-request/egress-policy",
    ClientError::InvalidRequest(ClientInput::StreamBudget) => "invalid-request/stream-budget",
    ClientError::Encode => "encode",
    ClientError::Dns => "dns",
    ClientError::Connect => "connect",
    ClientError::Tls => "tls",
    ClientError::Transport => "transport",
    ClientError::Timeout(ClientTimeout::Operation) => "timeout/operation",
    ClientError::Timeout(ClientTimeout::Attempt) => "timeout/attempt",
    ClientError::Timeout(ClientTimeout::Transport) => "timeout/transport",
    ClientError::Timeout(ClientTimeout::ResponseIdle) => "timeout/response-idle",
    ClientError::Redirect(ClientRedirect::InvalidLocation) => "redirect/invalid-location",
    ClientError::Redirect(ClientRedirect::OriginChanged) => "redirect/origin-changed",
    ClientError::Redirect(ClientRedirect::HopLimit) => "redirect/hop-limit",
    ClientError::Redirect(ClientRedirect::UnsafeMethod) => "redirect/unsafe-method",
    ClientError::Status(_) => "status",
    ClientError::Decode(ClientDecode::Utf8) => "decode/utf8",
    ClientError::Decode(ClientDecode::Json) => "decode/json",
    ClientError::ResponseTooLarge { .. } => "response-too-large",
    ClientError::Egress(ClientEgress::Scheme) => "egress/scheme",
    ClientError::Egress(ClientEgress::Address) => "egress/address",
    ClientError::Egress(ClientEgress::ResolutionLimit) => "egress/resolution-limit",
}

/// The compile-time half of the error contract.
fn assert_is_error<E: std::error::Error + Send + Sync + 'static>(_error: &E) {}

/// Require each value of one nested enum to print a name of its own.
fn assert_closed_names<T: Display + Debug>(values: &[T], label: &str) {
    let mut shown = BTreeSet::new();
    let mut debugged = BTreeSet::new();
    for value in values {
        let display = value.to_string();
        let debug = format!("{value:?}");
        assert!(!display.is_empty(), "{label}: an empty Display for {debug}");
        assert!(!debug.is_empty(), "{label}: an empty Debug for {display}");
        shown.insert(display);
        debugged.insert(debug);
    }
    assert_eq!(
        shown.len(),
        values.len(),
        "{label}: two values share a Display"
    );
    assert_eq!(
        debugged.len(),
        values.len(),
        "{label}: two values share a Debug"
    );
}

/// Require every variant of one nested enum to print a name of its own.
///
/// The list is the array the names are read from and the arms of a match with
/// no wildcard, so a variant missing from the list fails to compile.
macro_rules! assert_closed_enum {
    ($type:ident { $($variant:ident),+ $(,)? }) => {{
        let values = [$($type::$variant),+];
        for value in values {
            match value {
                $($type::$variant => (),)+
            }
        }
        assert_closed_names(&values, stringify!($type));
    }};
}

/// Every nested category is a value with its own safe name.
fn assert_nested_categories_are_named() {
    assert_closed_enum!(ClientInput {
        Method,
        Url,
        Header,
        ReservedHeader,
        Body,
        ResponseLimit,
        EgressPolicy,
        StreamBudget,
    });
    assert_closed_enum!(ClientTimeout {
        Operation,
        Attempt,
        Transport,
        ResponseIdle,
    });
    assert_closed_enum!(ClientRedirect {
        InvalidLocation,
        OriginChanged,
        HopLimit,
        UnsafeMethod,
    });
    assert_closed_enum!(ClientDecode { Utf8, Json });
    assert_closed_enum!(ClientEgress {
        Scheme,
        Address,
        ResolutionLimit,
    });
}

/// The rows name every declared category, and each row builds the one it names.
fn assert_every_category_has_a_row(rows: &[Row]) {
    let mut labels = BTreeSet::new();
    let mut covered = BTreeSet::new();
    for row in rows {
        assert!(
            labels.insert(row.label),
            "{}: two rows share this label",
            row.label
        );
        assert_eq!(
            category_of(&(row.build)()),
            row.category,
            "{}: the row builds another category",
            row.label
        );
        covered.insert(row.category);
    }
    assert_eq!(
        covered,
        BTreeSet::from(CATEGORIES),
        "the rows do not cover the declared categories"
    );
}

/// Each category prints a name of its own, carries its numeric fact, keeps no
/// raw source, and reads the same through `RuntimeError`.
fn assert_safe_formatting(rows: &[Row]) {
    let mut shown = BTreeSet::new();
    for row in rows {
        let category = (row.build)();
        assert_is_error(&category);
        let display = category.to_string();
        assert!(!display.is_empty(), "{}: an empty Display", row.label);
        assert!(
            shown.insert(display.clone()),
            "{}: another category already prints {display:?}",
            row.label
        );

        let error = RuntimeError::Client(category);
        assert!(
            error.to_string().contains(&display),
            "{}: RuntimeError prints {error} without the category {display:?}",
            row.label
        );
        assert_no_raw_source(&error, row.label);
        for rendering in renderings(&error, row.label) {
            assert!(!rendering.is_empty(), "{}: an empty rendering", row.label);
        }
    }

    let status = ClientError::Status(503).to_string();
    assert!(
        status.contains("503"),
        "a status failure names its status: {status}"
    );
    let limit = ClientError::ResponseTooLarge { limit: ROW_LIMIT }.to_string();
    assert!(
        limit.contains(&ROW_LIMIT.to_string()),
        "a response-limit failure names its limit: {limit}"
    );
}

/// What the configured mapper was handed for one refusal.
#[derive(Debug)]
struct Mapped {
    path: Box<str>,
    kind: RejectionKind,
    status: u16,
    message: Box<str>,
}

type Journal = Arc<Mutex<Vec<Mapped>>>;

/// The prefix every row's path has before its index.
const ROW_PREFIX: &str = "/client-failure/";

/// The path one row's failing route is served at.
fn row_path(index: usize) -> String {
    format!("{ROW_PREFIX}{index}")
}

/// The stage of a served request that returns the row's category.
#[derive(Clone, Copy, Debug)]
enum Stage {
    Handler,
    /// A middleware frame returns it, and the handler behind it is never
    /// entered. A middleware frame's own faults are middleware refusals, so
    /// only the outbound classification makes this one an internal-service
    /// refusal.
    Middleware,
}

/// The row a request path names, if it names one.
fn row_build(builds: &[fn() -> ClientError], path: &str) -> Option<fn() -> ClientError> {
    path.strip_prefix(ROW_PREFIX)?
        .parse::<usize>()
        .ok()
        .and_then(|index| builds.get(index).copied())
}

/// One route per row, each returning its category as a handler failure.
fn add_failing_handlers(router: &mut Router, rows: &[Row]) {
    for (index, row) in rows.iter().enumerate() {
        let build = row.build;
        router.get(&row_path(index), move |_: &Request| {
            std::future::ready(Err::<Response, RuntimeError>(RuntimeError::Client(build())))
        });
    }
}

/// One frame that returns the category of the row a path names, in front of
/// one route per row that records that it was entered. Returns that record.
fn add_failing_middleware(router: &mut Router, rows: &[Row]) -> Arc<AtomicBool> {
    let entered = Arc::new(AtomicBool::new(false));
    let builds: Arc<[fn() -> ClientError]> = rows.iter().map(|row| row.build).collect();
    router.use_middleware(move |req: &Request, _: Next| {
        let outcome: Result<Response, RuntimeError> = match row_build(&builds, req.path()) {
            Some(build) => Err(RuntimeError::Client(build())),
            None => Response::text(404, "no row"),
        };
        std::future::ready(outcome)
    });
    for index in 0..rows.len() {
        let entered = Arc::clone(&entered);
        router.get(&row_path(index), move |_: &Request| {
            entered.store(true, Ordering::SeqCst);
            std::future::ready(Response::text(200, "the handler was entered"))
        });
    }
    entered
}

/// One route per row, failing at `stage` with the row's category, behind a
/// mapper that records what it was handed. The middleware stage also returns
/// the record of a handler entered behind its failing frame.
fn failing_router(
    rows: &[Row],
    stage: Stage,
    journal: &Journal,
) -> (Router, Option<Arc<AtomicBool>>) {
    let mut router = Router::new();
    let entered = match stage {
        Stage::Handler => {
            add_failing_handlers(&mut router, rows);
            None
        }
        Stage::Middleware => Some(add_failing_middleware(&mut router, rows)),
    };
    let recorded = Arc::clone(journal);
    let router =
        router.rejection_mapper(move |rejection: &Rejection, context: &RejectionContext| {
            recorded
                .lock()
                .expect("the journal lock is not poisoned")
                .push(Mapped {
                    path: context.raw_path().into(),
                    kind: rejection.kind(),
                    status: rejection.status(),
                    message: rejection.message().into(),
                });
            Response::text(rejection.status(), rejection.message())
        });
    (router, entered)
}

/// Every category a handler or a middleware frame returns is the fixed
/// internal-service refusal.
///
/// The status a `Status` failure carries is the upstream's. It is never the
/// status the inbound peer is given, and a response-limit failure is not a
/// payload refusal of the inbound request.
fn assert_internal_service_refusals(rows: &[Row], stage: Stage) {
    let journal = Journal::default();
    let (router, entered) = failing_router(rows, stage, &journal);
    let driven = Arc::clone(&journal);
    let labels: Box<[&'static str]> = rows.iter().map(|row| row.label).collect();

    test_runtime()
        .run(move || {
            let addr = spawn_server(router);
            for (index, label) in labels.iter().enumerate() {
                let path = row_path(index);
                let label = format!("{label} at {stage:?}");
                let response = request(addr, "GET", &path, &[], &[], WIRE_TIMEOUT)
                    .unwrap_or_else(|error| panic!("{label}: the request failed: {error}"));
                assert_eq!(response.status, INTERNAL_STATUS, "{label}: wire status");
                assert_eq!(&*response.text(), FIXED_MESSAGE, "{label}: wire body");

                let mapped = driven
                    .lock()
                    .expect("the journal lock is not poisoned")
                    .pop()
                    .unwrap_or_else(|| panic!("{label}: the mapper was never called"));
                assert_eq!(&*mapped.path, path.as_str(), "{label}: mapped request");
                assert_eq!(
                    mapped.kind,
                    RejectionKind::InternalService,
                    "{label}: an outbound failure is an internal-service refusal"
                );
                assert_eq!(mapped.status, INTERNAL_STATUS, "{label}: mapped status");
                assert_eq!(
                    &*mapped.message, FIXED_MESSAGE,
                    "{label}: the mapper is handed the fixed safe message"
                );
            }
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");

    let unread = journal.lock().expect("the journal lock is not poisoned");
    assert!(
        unread.is_empty(),
        "the mapper was called for a request no row sent: {unread:?}"
    );
    if let Some(entered) = entered {
        assert!(
            !entered.load(Ordering::SeqCst),
            "{stage:?}: a handler behind the failing frame was entered"
        );
    }
}

/// A secret the refused client construction was given.
const AGENT_SECRET: &str = "agent-sentinel-2d70";

/// A real Reqwest client construction failure.
///
/// A user agent holding a line break is not a header value, so the build is
/// refused before any transport exists.
fn construction_failure() -> reqwest::Error {
    let refused = reqwest::Client::builder()
        .user_agent(format!("{AGENT_SECRET}\n"))
        .build();
    match refused {
        Err(error) => {
            assert!(
                error.is_builder(),
                "the fixture failure is not a construction failure: {error:?}"
            );
            error
        }
        Ok(_) => panic!("Reqwest built a client with an unrepresentable user agent"),
    }
}

/// The host the DNS row asks its own resolver for.
const LOOKUP_HOST: &str = "provider.invalid";

/// The path and query the DNS row's request target carries.
const LOOKUP_PATH_SECRET: &str = "lookup-path-sentinel-55e2";
const LOOKUP_QUERY_SECRET: &str = "lookup-query-sentinel-0b9c";

/// A secret the failing resolver's own error echoes.
const RESOLVER_SECRET: &str = "resolver-sentinel-c4a1";

/// A resolver that refuses every name, with an error that echoes the name.
struct RefusingResolver;

impl reqwest::dns::Resolve for RefusingResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let refused: Box<dyn std::error::Error + Send + Sync> = Box::new(LeakySource::echoing(
            format!("lookup of {} failed: {RESOLVER_SECRET}", name.as_str()),
        ));
        Box::pin(std::future::ready(Err::<reqwest::dns::Addrs, _>(refused)))
    }
}

/// The runtime one fixture request is driven on.
pub(crate) fn local_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime")
}

/// A real Reqwest DNS failure, from a lookup this test answers itself.
pub(crate) fn resolution_failure() -> reqwest::Error {
    local_runtime().block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(RefusingResolver)
            .build()
            .expect("a client over the refusing resolver");
        let url = format!("http://{LOOKUP_HOST}/{LOOKUP_PATH_SECRET}?token={LOOKUP_QUERY_SECRET}");
        match client.get(url).send().await {
            Err(error) => {
                assert!(
                    error.is_dns(),
                    "the fixture failure is not a DNS failure: {error:?}"
                );
                error
            }
            Ok(response) => panic!(
                "a refused lookup still answered {}",
                response.status().as_u16()
            ),
        }
    })
}

/// The host the connect-timeout row asks its own resolver for.
const STALLED_HOST: &str = "stalled.invalid";

/// The path and query the connect-timeout row's request target carries.
const STALLED_PATH_SECRET: &str = "stalled-path-sentinel-e08b";
const STALLED_QUERY_SECRET: &str = "stalled-query-sentinel-36fa";

/// The connect interval the stalled lookup outlasts.
const STALLED_CONNECT_BOUND: std::time::Duration = std::time::Duration::from_millis(50);

/// A resolver that never answers.
struct SilentResolver;

impl reqwest::dns::Resolve for SilentResolver {
    fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(std::future::pending())
    }
}

/// A real Reqwest connect-timeout failure, from a lookup that never answers.
///
/// The connect interval covers the lookup, so its expiry is both a timeout and
/// a connect failure, and only the order the projection reads them in decides
/// the category. No socket is opened, so the clock is paused and the interval
/// is the only thing the runtime can wait for.
pub(crate) fn connect_timeout_failure() -> reqwest::Error {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("a paused current-thread runtime");
    runtime.block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(SilentResolver)
            .connect_timeout(STALLED_CONNECT_BOUND)
            .build()
            .expect("a client over the silent resolver");
        let url =
            format!("http://{STALLED_HOST}/{STALLED_PATH_SECRET}?token={STALLED_QUERY_SECRET}");
        match client.get(url).send().await {
            Err(error) => {
                assert!(
                    error.is_timeout(),
                    "the fixture failure is not a timeout: {error:?}"
                );
                assert!(
                    error.is_connect(),
                    "the fixture failure is not a connect failure: {error:?}"
                );
                assert!(
                    !error.is_dns(),
                    "the fixture failure is a DNS failure: {error:?}"
                );
                error
            }
            Ok(response) => panic!(
                "a lookup that never answered still answered {}",
                response.status().as_u16()
            ),
        }
    })
}

/// The path and query the refused request target carries.
const TARGET_PATH_SECRET: &str = "target-path-sentinel-9d3e";
const TARGET_QUERY_SECRET: &str = "target-query-sentinel-41f7";

/// A real Reqwest request construction failure that echoes its target.
///
/// Reqwest sends only HTTP schemes, so the request is refused before any
/// transport is asked, and the refusal keeps the URL it was given.
pub(crate) fn target_failure() -> reqwest::Error {
    let url = format!("ftp://{LOOKUP_HOST}/{TARGET_PATH_SECRET}?token={TARGET_QUERY_SECRET}");
    let refused = local_runtime().block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a client with no proxy");
        client.get(url).send().await
    });
    match refused {
        Err(error) => {
            assert!(
                error.is_builder(),
                "the fixture failure is not a construction failure: {error:?}"
            );
            error
        }
        Ok(response) => panic!(
            "a scheme Reqwest cannot send still answered {}",
            response.status().as_u16()
        ),
    }
}

/// The path and query every redirect of the looping peer names.
const LOOP_PATH_SECRET: &str = "loop-path-sentinel-7c15";
const LOOP_QUERY_SECRET: &str = "loop-query-sentinel-a26d";

/// The longest the looping peer waits for one request head.
const LOOP_READ_BOUND: std::time::Duration = WIRE_TIMEOUT;

/// A local peer that answers every request with a redirect to itself.
struct LoopingPeer {
    addr: SocketAddr,
    stopped: Arc<AtomicBool>,
    serving: JoinHandle<usize>,
}

impl LoopingPeer {
    /// Bind a loopback port and answer on it until [`Self::stop`].
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let addr = listener.local_addr().expect("the listener has an address");
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let serving = std::thread::spawn(move || {
            let mut answered = 0_usize;
            for accepted in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let stream = accepted.expect("the looping peer accepted a connection");
                answer_with_redirect(stream);
                answered += 1;
            }
            answered
        });
        Self {
            addr,
            stopped,
            serving,
        }
    }

    /// The target every answer of this peer redirects to.
    fn url(&self) -> String {
        format!(
            "http://{}/{LOOP_PATH_SECRET}?token={LOOP_QUERY_SECRET}",
            self.addr
        )
    }

    /// Stop accepting, join the peer, and return how many requests it answered.
    ///
    /// The flag is read after each accept, so one connection of this test's
    /// own wakes the thread to read it.
    fn stop(self) -> usize {
        self.stopped.store(true, Ordering::SeqCst);
        drop(TcpStream::connect(self.addr).expect("the looping peer is still listening"));
        self.serving.join().expect("the looping peer did not panic")
    }
}

/// Read one request head and answer it with a redirect to the same target.
fn answer_with_redirect(mut stream: TcpStream) {
    if let Err(error) = read_head(&mut stream, LOOP_READ_BOUND) {
        panic!("the looping peer read no whole request head: {error}");
    }
    let answer = format!(
        "HTTP/1.1 302 Found\r\nLocation: /{LOOP_PATH_SECRET}?token={LOOP_QUERY_SECRET}\r\n\
         Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(answer.as_bytes())
        .expect("the looping peer wrote its redirect");
}

/// A real Reqwest redirect failure, from a local peer that never stops
/// redirecting.
///
/// The production client follows no redirect through Reqwest, so it never
/// returns this failure. The fixture's own client keeps Reqwest's default
/// redirect policy, and the hop ceiling is that client's. The row proves the
/// projection arm only.
pub(crate) fn redirect_failure() -> reqwest::Error {
    let peer = LoopingPeer::start();
    let outcome = local_runtime().block_on(async {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("a client with the default redirect policy");
        client.get(peer.url()).send().await
    });
    let answered = peer.stop();
    match outcome {
        Err(error) => {
            assert!(
                error.is_redirect(),
                "the fixture failure is not a redirect failure: {error:?}"
            );
            assert!(
                answered > 1,
                "the peer answered {answered} request(s), so no redirect was followed"
            );
            error
        }
        Ok(response) => panic!(
            "an endless redirect still answered {}",
            response.status().as_u16()
        ),
    }
}

/// One real Reqwest failure the production projection is fed.
struct ProjectionRow {
    label: &'static str,
    raw: fn() -> reqwest::Error,
    /// The name [`category_of`] must give the projected value.
    category: &'static str,
    /// What the raw failure carried that the projection may not repeat.
    secrets: &'static [&'static str],
    /// The secret the raw failure must itself print. A row without one proves
    /// its category and its closed source chain, not a redaction.
    printed: Option<&'static str>,
}

const PROJECTION_ROWS: [ProjectionRow; 8] = [
    // Reqwest reports an unrepresentable user agent without repeating it, so
    // this row has no raw rendering to redact.
    ProjectionRow {
        label: "construction failure",
        raw: construction_failure,
        category: "configuration",
        secrets: &[AGENT_SECRET],
        printed: None,
    },
    ProjectionRow {
        label: "target failure",
        raw: target_failure,
        category: "configuration",
        secrets: &[LOOKUP_HOST, TARGET_PATH_SECRET, TARGET_QUERY_SECRET],
        printed: Some(TARGET_PATH_SECRET),
    },
    ProjectionRow {
        label: "DNS failure",
        raw: resolution_failure,
        category: "dns",
        secrets: &[
            LOOKUP_HOST,
            LOOKUP_PATH_SECRET,
            LOOKUP_QUERY_SECRET,
            RESOLVER_SECRET,
        ],
        printed: Some(LOOKUP_PATH_SECRET),
    },
    // The one row whose failure is both a timeout and a connect failure.
    ProjectionRow {
        label: "connect timeout",
        raw: connect_timeout_failure,
        category: "timeout/transport",
        secrets: &[STALLED_HOST, STALLED_PATH_SECRET, STALLED_QUERY_SECRET],
        printed: Some(STALLED_PATH_SECRET),
    },
    ProjectionRow {
        label: "redirect failure",
        raw: redirect_failure,
        category: "redirect/hop-limit",
        secrets: &[LOOP_PATH_SECRET, LOOP_QUERY_SECRET],
        printed: Some(LOOP_PATH_SECRET),
    },
    // The next three failures come from async fixtures, so each row drives its
    // own on a runtime of its own.
    ProjectionRow {
        label: "refused connect",
        raw: || local_runtime().block_on(retry_delays::refused_connect_error()),
        category: "connect",
        secrets: &[retry_delays::REFUSED_PATH_SECRET],
        printed: Some(retry_delays::REFUSED_PATH_SECRET),
    },
    ProjectionRow {
        label: "request closed unanswered",
        raw: || local_runtime().block_on(retry_delays::ambiguous_transport_error()),
        category: "transport",
        secrets: &[retry_delays::AMBIGUOUS_PATH_SECRET],
        printed: Some(retry_delays::AMBIGUOUS_PATH_SECRET),
    },
    ProjectionRow {
        label: "untrusted certificate",
        raw: || local_runtime().block_on(retry_delays::untrusted_tls_error()),
        category: "tls",
        secrets: &[retry_delays::UNTRUSTED_PATH_SECRET],
        printed: Some(retry_delays::UNTRUSTED_PATH_SECRET),
    },
];

/// A table with no rows would project no failure and still report success.
const _: () = assert!(!PROJECTION_ROWS.is_empty());

/// The production projection gives each real Reqwest failure its category, and
/// keeps nothing the raw failure printed.
fn assert_projection_is_typed_and_safe(rows: &[ProjectionRow]) {
    for row in rows {
        let label = row.label;
        let raw = (row.raw)();
        let raw_renderings = renderings(&raw, label);
        match row.printed {
            Some(secret) => assert!(
                raw_renderings
                    .iter()
                    .any(|rendering| rendering.contains(secret)),
                "{label}: the raw failure printed no {secret:?}, so the row proves no redaction"
            ),
            None => assert!(
                row.secrets.iter().all(|secret| raw_renderings
                    .iter()
                    .all(|rendering| !rendering.contains(secret))),
                "{label}: the raw failure prints a secret, so the row must name it as printed"
            ),
        }
        let projected = client_error_projection(&raw);
        assert_eq!(
            category_of(&projected),
            row.category,
            "{label}: projected {projected:?}"
        );
        let error = RuntimeError::Client(projected);
        assert_safe(&error, row.secrets, label);
    }
}

/// The private address the egress rows except is [`EXCEPTED`]. This is the
/// one beside it.
const REFUSED_TEXT: &str = "10.20.30.41";

/// The port the excepted origin and every egress target name.
const EXCEPTED_PORT: u16 = 8080;

/// The text the egress rows build their policy and their targets from.
const EXCEPTION_HOST_SECRET: &str = "exception-host-sentinel-6b1f.internal";
const EGRESS_HOST_SECRET: &str = "egress-host-sentinel-83c2.internal";
const EGRESS_USER_SECRET: &str = "egress-user-sentinel-5a90";
const EGRESS_PASSWORD_SECRET: &str = "egress-password-sentinel-d417";
const EGRESS_PATH_SECRET: &str = "egress-path-sentinel-2e6c";
const EGRESS_QUERY_SECRET: &str = "egress-query-sentinel-f03b";

/// Everything an egress refusal was built from and may not repeat.
const EGRESS_SECRETS: [&str; 8] = [
    EXCEPTION_HOST_SECRET,
    EGRESS_HOST_SECRET,
    EGRESS_USER_SECRET,
    EGRESS_PASSWORD_SECRET,
    EGRESS_PATH_SECRET,
    EGRESS_QUERY_SECRET,
    EXCEPTED,
    REFUSED_TEXT,
];

/// The restricted policy with one exception at [`EXCEPTED`].
fn excepting_policy() -> EgressPolicy {
    let origin = format!("http://{EXCEPTION_HOST_SECRET}:{EXCEPTED_PORT}");
    excepting(EgressPolicy::public_internet(), &origin, ip(EXCEPTED))
}

/// The failure a call returned.
fn refusal<T>(result: Result<T, RuntimeError>, label: &str) -> RuntimeError {
    match result {
        Err(error) => error,
        Ok(_) => panic!("{label}: the call was not refused"),
    }
}

/// The category a refusal carries.
fn refusal_category(error: &RuntimeError, label: &str) -> &'static str {
    match error {
        RuntimeError::Client(category) => category_of(category),
        other => panic!("{label}: the refusal is not a client failure: {other:?}"),
    }
}

/// A refused exception, built from an origin that holds every target secret.
fn refused_exception() -> RuntimeError {
    let origin = format!(
        "http://{EGRESS_USER_SECRET}:{EGRESS_PASSWORD_SECRET}@{EGRESS_HOST_SECRET}\
         /{EGRESS_PATH_SECRET}?token={EGRESS_QUERY_SECRET}"
    );
    let added: Result<EgressPolicy, RuntimeError> =
        excepting_policy().allow_destination(&origin, ip(REFUSED_TEXT));
    refusal(added, "an origin with userinfo, a path, and a query")
}

/// A target at the excepted port, with a path and a query, under `scheme`.
fn egress_target(scheme: &str) -> String {
    format!(
        "{scheme}://{EGRESS_HOST_SECRET}:{EXCEPTED_PORT}\
         /{EGRESS_PATH_SECRET}?token={EGRESS_QUERY_SECRET}"
    )
}

/// One refusal a real policy returned, with the category and the outcome it
/// must carry.
struct EgressRefusal {
    label: &'static str,
    error: RuntimeError,
    category: &'static str,
    outcome: &'static str,
}

/// The three refusals a real policy returns, each built from secret text.
fn egress_refusals(policy: &EgressPolicy) -> [EgressRefusal; 3] {
    let address: Result<(), RuntimeError> =
        decide(policy, &egress_target("http"), ip(REFUSED_TEXT));
    let secure = excepting_policy().https_only();
    let scheme: Result<(), RuntimeError> = decide(&secure, &egress_target("http"), ip(EXCEPTED));
    [
        EgressRefusal {
            label: "refused exception",
            error: refused_exception(),
            category: "invalid-request/egress-policy",
            outcome: "invalid-request",
        },
        EgressRefusal {
            label: "refused address",
            error: refusal(address, "a private address with no exception"),
            category: "egress/address",
            outcome: "egress",
        },
        EgressRefusal {
            label: "refused scheme",
            error: refusal(scheme, "an HTTP target under HTTPS-only"),
            category: "egress/scheme",
            outcome: "egress",
        },
    ]
}

/// The refusals a real policy returns keep their closed category, record
/// their outcome, and repeat nothing they were built from. Neither does the
/// policy's own `Debug`.
fn assert_egress_refusals_keep_no_text() {
    let policy = excepting_policy();
    for row in egress_refusals(&policy) {
        let (label, error) = (row.label, &row.error);
        assert_eq!(
            refusal_category(error, label),
            row.category,
            "{label}: {error:?}"
        );
        assert_eq!(
            closed_outcome(error, label),
            row.outcome,
            "{label}: outcome"
        );
        assert_safe(error, &EGRESS_SECRETS, label);
    }

    for rendering in [format!("{policy:?}"), format!("{policy:#?}")] {
        assert!(!rendering.is_empty(), "the policy has an empty Debug");
        let repeated = EGRESS_SECRETS
            .iter()
            .find(|secret| rendering.contains(**secret));
        assert_eq!(repeated, None, "the policy's Debug repeats it: {rendering}");
    }
}

/// Every egress category the rows build.
///
/// The rows cover every `ClientEgress` reason, so a new reason is asked the
/// replay question once it has its row.
fn egress_categories(rows: &[Row]) -> Box<[ClientError]> {
    let refused: Box<[ClientError]> = rows
        .iter()
        .map(|row| (row.build)())
        .filter(|category| matches!(category, ClientError::Egress(_)))
        .collect();
    assert!(!refused.is_empty(), "no row builds an egress refusal");
    refused
}

/// Require that no category of `refused` is sent again, for any method under
/// any permission.
///
/// `control` is a category a permission does replay. It is asked beside each
/// row and must follow the permission, so a decision that refuses everything
/// fails here.
fn assert_never_replayed(control: ClientError, control_label: &str, refused: &[ClientError]) {
    let methods = retry_delays::methods();
    let asked = methods
        .iter()
        .flat_map(|method| retry_delays::PERMISSIONS.map(|policy| (method, policy)));
    for (method, policy) in asked {
        assert_eq!(
            client_retryable_failure(control, method, policy),
            retry_delays::permits(policy, method),
            "{control_label}, {method} under {policy:?}"
        );
        let replayed = refused
            .iter()
            .find(|category| client_retryable_failure(**category, method, policy));
        assert_eq!(replayed, None, "{method} under {policy:?} was replayed");
    }
}

/// No egress refusal is sent again, for any method under any permission.
///
/// The refused dial is asked beside each, so a decision that refuses
/// everything proves nothing here.
fn assert_egress_refusals_never_replay(rows: &[Row]) {
    assert_never_replayed(
        ClientError::Connect,
        "a refused dial",
        &egress_categories(rows),
    );
}

/// The two categories a stream adds. Neither is a pre-head outcome another
/// send could change.
fn stream_categories() -> [ClientError; 2] {
    [
        ClientError::InvalidRequest(ClientInput::StreamBudget),
        ClientError::Timeout(ClientTimeout::ResponseIdle),
    ]
}

/// Neither stream category is sent again, for any method under any
/// permission.
///
/// The attempt timer is asked beside each, so a decision that refuses every
/// timeout proves nothing here. A quiet body is read after the final head, and
/// no failure after that head starts another send.
fn assert_stream_categories_never_replay() {
    assert_never_replayed(
        ClientError::Timeout(ClientTimeout::Attempt),
        "an expired attempt",
        &stream_categories(),
    );
}

/// 1.T2, 4.T3
#[test]
fn client_error_categories_map_to_internal_service() {
    assert_nested_categories_are_named();
    assert_every_category_has_a_row(&ROWS);
    assert_safe_formatting(&ROWS);
    assert_projection_is_typed_and_safe(&PROJECTION_ROWS);
    assert_egress_refusals_keep_no_text();
    assert_egress_refusals_never_replay(&ROWS);
    assert_stream_categories_never_replay();
    assert_internal_service_refusals(&ROWS, Stage::Handler);
    assert_internal_service_refusals(&ROWS, Stage::Middleware);
}

/// Every outcome a completion event may record.
const OUTCOMES: [&str; 14] = [
    "response",
    "configuration",
    "no-runtime",
    "invalid-request",
    "dns",
    "connect",
    "tls",
    "transport",
    "timeout",
    "redirect",
    "response-too-large",
    "egress",
    "cancelled",
    "abandoned",
];

/// The outcomes no returned failure records: an answer, and a pending call
/// that was dropped. Neither is a failure, so the projection never names one.
const UNFAILED_OUTCOMES: [&str; 2] = ["response", "abandoned"];

/// The outcome an operation that ends with `error` records, or `None` for a
/// category no operation ends with.
///
/// A request that does not serialize is refused by its builder, before any
/// operation. A status and a decoding failure come from a local conversion of
/// an answer the operation already returned. The match has no wildcard arm, so
/// a new category fails to compile until it is given an outcome or named
/// here.
fn outcome_of(error: &ClientError) -> Option<&'static str> {
    match error {
        ClientError::Configuration => Some("configuration"),
        ClientError::InvalidRequest(
            ClientInput::Method
            | ClientInput::Url
            | ClientInput::Header
            | ClientInput::ReservedHeader
            | ClientInput::Body
            | ClientInput::ResponseLimit
            | ClientInput::EgressPolicy
            | ClientInput::StreamBudget,
        ) => Some("invalid-request"),
        ClientError::Dns => Some("dns"),
        ClientError::Connect => Some("connect"),
        ClientError::Tls => Some("tls"),
        ClientError::Transport => Some("transport"),
        ClientError::Timeout(
            ClientTimeout::Operation
            | ClientTimeout::Attempt
            | ClientTimeout::Transport
            | ClientTimeout::ResponseIdle,
        ) => Some("timeout"),
        ClientError::Redirect(
            ClientRedirect::InvalidLocation
            | ClientRedirect::OriginChanged
            | ClientRedirect::HopLimit
            | ClientRedirect::UnsafeMethod,
        ) => Some("redirect"),
        ClientError::ResponseTooLarge { .. } => Some("response-too-large"),
        ClientError::Egress(
            ClientEgress::Scheme | ClientEgress::Address | ClientEgress::ResolutionLimit,
        ) => Some("egress"),
        ClientError::Encode
        | ClientError::Status(_)
        | ClientError::Decode(ClientDecode::Utf8 | ClientDecode::Json) => None,
    }
}

/// Require the projection to give `error` a failure outcome of the closed
/// vocabulary, and return it.
///
/// The check holds for every failure, including one no operation ends with: a
/// label outside the vocabulary could carry a status, a limit, or source text.
fn closed_outcome(error: &RuntimeError, label: &str) -> &'static str {
    let outcome = client_terminal_outcome(error);
    assert!(
        OUTCOMES.contains(&outcome),
        "{label}: {outcome:?} is outside the closed vocabulary"
    );
    assert!(
        !UNFAILED_OUTCOMES.contains(&outcome),
        "{label}: a failure was projected as {outcome:?}"
    );
    outcome
}

/// Every category projects to its outcome, the three runtime failures an
/// operation returns project to theirs, and together they reach the whole
/// vocabulary.
fn assert_categories_project_closed_outcomes(rows: &[Row]) {
    let mut reached = BTreeSet::from(UNFAILED_OUTCOMES);
    for row in rows {
        let category = (row.build)();
        let outcome = closed_outcome(&RuntimeError::Client(category), row.label);
        if let Some(expected) = outcome_of(&category) {
            assert_eq!(outcome, expected, "{}: projected outcome", row.label);
            reached.insert(outcome);
        }
    }
    for (label, error, expected) in [
        ("cancelled", RuntimeError::Cancelled, "cancelled"),
        ("no runtime", RuntimeError::NoRuntime, "no-runtime"),
        // A stream its runtime refuses to admit. The word is the least
        // specific failure, and the reference documents it.
        ("closed scope", RuntimeError::ScopeClosed, "transport"),
    ] {
        assert_eq!(
            closed_outcome(&error, label),
            expected,
            "{label}: projected outcome"
        );
        reached.insert(expected);
    }
    assert_eq!(
        reached,
        BTreeSet::from(OUTCOMES),
        "the failures do not reach the declared outcomes"
    );
}

/// Each real Reqwest failure projects, through both production projections,
/// to the outcome of its category.
fn assert_raw_failures_project_closed_outcomes(rows: &[ProjectionRow]) {
    for row in rows {
        let category = client_error_projection(&(row.raw)());
        let outcome = closed_outcome(&RuntimeError::Client(category), row.label);
        assert_eq!(
            Some(outcome),
            outcome_of(&category),
            "{}: projected {category:?}",
            row.label
        );
    }
}

/// A request through the public builder, with automatic trace context
/// suppressed.
fn unsent<'a>(client: &'a Client, method: &str, url: &str) -> ClientRequest<'a> {
    client
        .request(method, url)
        .unwrap_or_else(|error| panic!("{method} request was refused: {error:?}"))
        .trace_context(false)
}

/// Three public requests end before any send, and the failure each returns
/// projects to the outcome its end records.
///
/// The target is a listener nothing accepts on, so an empty backlog proves no
/// row dialed. A deadline taken before the first poll has already expired
/// when the operation starts.
fn assert_unsent_requests_project_their_outcome() {
    let target = UndialedListener::bind();
    let url = format!(
        "http://{}/{TARGET_PATH_SECRET}?token={TARGET_QUERY_SECRET}",
        target.addr()
    );
    let client: Client = camber::http::client();

    let unexecuted: Option<Result<ClientResponse, RuntimeError>> =
        unsent(&client, "GET", &url).send().now_or_never();
    let (refused, expired) = local_runtime().block_on(async {
        let refused = unsent(&client, "TRACE", &url).body("probe").send().await;
        let expired = unsent(&client, "GET", &url)
            .deadline(std::time::Instant::now())
            .send()
            .await;
        (refused, expired)
    });

    let unexecuted = unexecuted.expect("a call with no executor returns at its first poll");
    for (label, result, expected) in [
        ("no executor", unexecuted, "no-runtime"),
        ("TRACE with a body", refused, "invalid-request"),
        ("expired deadline", expired, "timeout"),
    ] {
        let error = refusal(result, label);
        assert_eq!(
            closed_outcome(&error, label),
            expected,
            "{label}: returned {error:?}"
        );
        assert_secret_free(&error, &[TARGET_PATH_SECRET, TARGET_QUERY_SECRET], label);
    }
    target.assert_undialed("a request that ends before its first send");
}

/// 7.T3
#[test]
fn client_terminal_vocabulary_projects_only_closed_facts() {
    assert_categories_project_closed_outcomes(&ROWS);
    assert_raw_failures_project_closed_outcomes(&PROJECTION_ROWS);
    assert_unsent_requests_project_their_outcome();
}
