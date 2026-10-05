//! 17.T1: every native tonic RPC form through Camber's own HTTP/2 serving.
//!
//! Unary, client-streaming, server-streaming, and bidirectional calls cross the
//! production middleware gate into `GrpcRouter` over real HTTP/2. A guard that
//! refuses an uncredentialed call answers before any form's method is entered.
//! A credentialed call keeps its metadata on the head and its status in the
//! trailers tonic wrote, whether that status is success or a typed refusal
//! raised after a streamed message. A service no router registered is answered
//! `200` with `grpc-status: 12`, recorded once, and handed to no mapper.
//!
//! This is regression proof over the existing serving path. Every row runs and
//! is torn down before any failure is reported.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use camber::http::mock::ScopedUnwatched;
use camber::http::{GrpcRouter, Next, Rejection, RejectionContext, Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::future::Either;
use tonic::transport::Channel;

use crate::common;
use crate::grpc_forms::{
    ECHO_KEY, FAIL_CODE, FAIL_MESSAGE, FAIL_NAME, Form, FormEntries, FormsService, GRPC_HEADERS,
    HelloReply, HelloRequest, NativeFormsClient, REQUEST_KEY, TRAILER_KEY, hello_frame,
};
use crate::http as http_support;
use crate::integration_rows::{Row, all, assert_verdicts, bounded, expect, expect_eq};
use crate::operation_completion::{
    Expected, Recorded, assert_event_matches, moved, only_completion,
};

/// How long one call, scrape wait, or teardown here may take.
const BOUND: Duration = Duration::from_secs(10);

/// How long the isolated child may take to run every row.
const MATRIX_BOUND: Duration = Duration::from_secs(60);

/// The credential the guard admits a gRPC call on.
const AUTH_HEADER: &str = "authorization";
const AUTH_VALUE: &str = "Bearer native-forms";

/// The path prefix the guard protects: every service in the fixture package.
const GUARDED_PREFIX: &str = "/greeter.";

/// A method on a service no router registered, inside the guarded package.
const UNKNOWN_PATH: &str = "/greeter.Missing/Method";

/// A plain path nothing routes, refused through the mapper.
const UNROUTED_PATH: &str = "/native-forms-unrouted";

/// The status tonic's own contract names a missing service with.
const UNIMPLEMENTED: &str = "12";

/// The served fixture: the guarded router, the form entries, and the mapper
/// count every row reads.
struct FormsFixture {
    server: http_support::ObservedServer<ScopedUnwatched>,
    entries: Arc<FormEntries>,
    mapped: Arc<AtomicUsize>,
}

impl FormsFixture {
    fn serve() -> Self {
        let entries = Arc::new(FormEntries::default());
        let mapped = Arc::new(AtomicUsize::new(0));
        let mut router = Router::new();
        router.use_middleware(guard);
        router.grpc(GrpcRouter::new().add_service(FormsService::serve(&entries)));
        let router = router.rejection_mapper(counting_mapper(&mapped));
        Self {
            server: http_support::reserve_unwatched().serve(router),
            entries,
            mapped,
        }
    }

    fn mapped(&self) -> usize {
        self.mapped.load(Ordering::SeqCst)
    }

    async fn client(&self) -> NativeFormsClient<Channel> {
        let channel = Channel::from_shared(format!("http://{}", self.server.addr()))
            .expect("the fixture endpoint is a valid URI")
            .connect()
            .await
            .expect("the gRPC channel connects to the fixture");
        NativeFormsClient::new(channel)
    }

    /// Stop the server under its bound, naming a failed stop as a row.
    fn tear_down(self) -> Row {
        self.server
            .shutdown_bounded(BOUND)
            .map_err(|error| format!("the forms server did not stop gracefully: {error}"))
    }
}

/// Refuse an uncredentialed call into the fixture package before tonic.
fn guard(
    request: &Request,
    next: Next,
) -> Either<camber::http::ResponseFuture, std::future::Ready<Response>> {
    let guarded = request.path().starts_with(GUARDED_PREFIX);
    let credentialed = request
        .headers()
        .any(|(name, value)| name.eq_ignore_ascii_case(AUTH_HEADER) && value == AUTH_VALUE);
    match (guarded, credentialed) {
        (true, false) => Either::Right(std::future::ready(unauthenticated())),
        _ => Either::Left(next.call(request)),
    }
}

/// The refusal the guard answers in gRPC's own vocabulary.
fn unauthenticated() -> Response {
    Response::text(401, "unauthenticated")
        .expect("a valid refusal status")
        .with_content_type("application/grpc")
        .with_header("grpc-status", "16")
}

/// A mapper that counts every refusal it is handed.
fn counting_mapper(
    mapped: &Arc<AtomicUsize>,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    let mapped = Arc::clone(mapped);
    move |rejection: &Rejection, context: &RejectionContext| {
        mapped.fetch_add(1, Ordering::SeqCst);
        common::naming(Response::text(rejection.status(), "mapped"), context)
    }
}

/// One request message named `name`.
fn hello(name: &str) -> HelloRequest {
    HelloRequest { name: name.into() }
}

/// A request stream carrying one message per name.
fn hellos(names: &[&str]) -> tokio_stream::Iter<std::vec::IntoIter<HelloRequest>> {
    tokio_stream::iter(names.iter().map(|name| hello(name)).collect::<Vec<_>>())
}

/// `message` as a call carrying the credential and its form's metadata.
fn credentialed<T>(form: Form, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        AUTH_HEADER,
        AUTH_VALUE
            .parse()
            .expect("the credential is valid metadata"),
    );
    request.metadata_mut().insert(
        REQUEST_KEY,
        form.name().parse().expect("a form name is valid metadata"),
    );
    request
}

/// The ASCII value `metadata` carries under `key`, if any.
fn metadata_value<'a>(metadata: &'a tonic::metadata::MetadataMap, key: &str) -> Option<&'a str> {
    metadata.get(key).and_then(|value| value.to_str().ok())
}

/// Every reply message a streamed response produced before its end.
async fn drained(
    stream: &mut tonic::Streaming<HelloReply>,
) -> Result<Box<[Box<str>]>, tonic::Status> {
    let mut messages = Vec::new();
    while let Some(reply) = stream.message().await? {
        messages.push(reply.message.into_boxed_str());
    }
    Ok(messages.into_boxed_slice())
}

/// The status, message, and form-naming metadata one refusal carried.
fn refusal_checks(form: Form, status: &tonic::Status) -> Row {
    all([
        expect_eq("refusal code", status.code(), FAIL_CODE),
        expect_eq("refusal message", status.message(), FAIL_MESSAGE),
        expect_eq(
            "refusal metadata",
            metadata_value(status.metadata(), TRAILER_KEY),
            Some(form.name()),
        ),
    ])
}

/// A single-reply call that must have been refused as `form`.
fn refused(form: Form, outcome: Result<tonic::Response<HelloReply>, tonic::Status>) -> Row {
    match outcome {
        Ok(answered) => Err(format!(
            "the refused {} call succeeded: {answered:?}",
            form.name()
        )),
        Err(status) => refusal_checks(form, &status),
    }
}

/// The head metadata and success trailer one completed call carried.
fn success_checks(
    form: Form,
    head: &tonic::metadata::MetadataMap,
    trailers: Option<&tonic::metadata::MetadataMap>,
) -> Row {
    all([
        expect_eq(
            "echoed head metadata",
            metadata_value(head, ECHO_KEY),
            Some(form.name()),
        ),
        expect_eq(
            "success trailer",
            trailers.and_then(|trailers| metadata_value(trailers, "grpc-status")),
            Some("0"),
        ),
    ])
}

/// Require exactly one entry into `form`'s method across `drive`.
fn entered_once(fixture: &FormsFixture, form: Form, drive: impl FnOnce() -> Row) -> Row {
    let before = fixture.entries.entered(form);
    let driven = drive();
    all([
        driven,
        expect_eq("method entries", fixture.entries.entered(form), before + 1),
    ])
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Without the credential, every form is refused before its method runs.
fn uncredentialed_calls_enter_no_form(fixture: &FormsFixture) -> Row {
    let codes = bounded("the uncredentialed calls", BOUND, async {
        let mut client = fixture.client().await;
        [
            client.unary(hello("anonymous")).await.map(|_| ()),
            client
                .client_streaming(hellos(&["anonymous"]))
                .await
                .map(|_| ()),
            client
                .server_streaming(hello("anonymous"))
                .await
                .map(|_| ()),
            client
                .bidirectional(hellos(&["anonymous"]))
                .await
                .map(|_| ()),
        ]
        .map(|outcome| outcome.map_err(|status| status.code()))
    })?;
    all(Form::ALL.into_iter().zip(codes).map(|(form, code)| {
        all([
            expect_eq(form.name(), code, Err(tonic::Code::Unauthenticated)),
            expect_eq(
                &format!("{} entries behind the refusing guard", form.name()),
                fixture.entries.entered(form),
                0,
            ),
        ])
    }))
}

fn unary_success_keeps_metadata_and_trailer(fixture: &FormsFixture) -> Row {
    let form = Form::Unary;
    entered_once(fixture, form, || {
        let answered = bounded("the unary call", BOUND, async {
            let mut client = fixture.client().await;
            client.unary(credentialed(form, hello("unary"))).await
        })?
        .map_err(|status| format!("the unary call failed: {status:?}"))?;
        // A unary client merges the trailers into the response metadata.
        all([
            success_checks(form, answered.metadata(), Some(answered.metadata())),
            expect_eq(
                "reply",
                answered.into_inner().message.as_str(),
                "Hello, unary!",
            ),
        ])
    })
}

fn unary_refusal_keeps_its_status(fixture: &FormsFixture) -> Row {
    let form = Form::Unary;
    entered_once(fixture, form, || {
        let outcome = bounded("the refused unary call", BOUND, async {
            let mut client = fixture.client().await;
            client.unary(credentialed(form, hello(FAIL_NAME))).await
        })?;
        refused(form, outcome)
    })
}

fn client_streaming_success_keeps_metadata_and_trailer(fixture: &FormsFixture) -> Row {
    let form = Form::ClientStreaming;
    entered_once(fixture, form, || {
        let answered = bounded("the client-streaming call", BOUND, async {
            let mut client = fixture.client().await;
            client
                .client_streaming(credentialed(form, hellos(&["first", "second"])))
                .await
        })?
        .map_err(|status| format!("the client-streaming call failed: {status:?}"))?;
        all([
            success_checks(form, answered.metadata(), Some(answered.metadata())),
            expect_eq(
                "reply over every uploaded message",
                answered.into_inner().message.as_str(),
                "Hello, first,second!",
            ),
        ])
    })
}

fn client_streaming_refusal_keeps_its_status(fixture: &FormsFixture) -> Row {
    let form = Form::ClientStreaming;
    entered_once(fixture, form, || {
        let outcome = bounded("the refused client-streaming call", BOUND, async {
            let mut client = fixture.client().await;
            client
                .client_streaming(credentialed(form, hellos(&["first", FAIL_NAME])))
                .await
        })?;
        refused(form, outcome)
    })
}

/// A completed streamed response: its head, its messages, and its trailers.
struct Streamed {
    head: tonic::metadata::MetadataMap,
    messages: Result<Box<[Box<str>]>, tonic::Status>,
    trailers: Result<Option<tonic::metadata::MetadataMap>, tonic::Status>,
}

/// One streamed call's outcome, as the generated client hands it back.
type StreamedOutcome = Result<tonic::Response<tonic::Streaming<HelloReply>>, tonic::Status>;

/// The head metadata of a streamed response, and the stream behind it.
fn opened(
    response: StreamedOutcome,
) -> Result<(tonic::metadata::MetadataMap, tonic::Streaming<HelloReply>), String> {
    let response = response.map_err(|status| format!("no streamed head: {status:?}"))?;
    let head = response.metadata().clone();
    Ok((head, response.into_inner()))
}

/// Read one streamed response to its end.
async fn streamed(response: StreamedOutcome) -> Result<Streamed, String> {
    let (head, mut stream) = opened(response)?;
    let messages = drained(&mut stream).await;
    let trailers = match messages {
        Ok(_) => stream.trailers().await,
        Err(_) => Ok(None),
    };
    Ok(Streamed {
        head,
        messages,
        trailers,
    })
}

/// A streamed success: every message, then a `grpc-status: 0` trailer.
fn streamed_success(form: Form, answered: &Streamed, expected: &[&str]) -> Row {
    let messages = answered
        .messages
        .as_ref()
        .map_err(|status| format!("the stream failed: {status:?}"))?;
    let trailers = answered
        .trailers
        .as_ref()
        .map_err(|status| format!("the trailers failed: {status:?}"))?;
    all([
        success_checks(form, &answered.head, trailers.as_ref()),
        expect_eq(
            "streamed replies",
            messages.iter().map(Box::as_ref).collect::<Vec<_>>(),
            expected.to_vec(),
        ),
    ])
}

/// A streamed refusal: the head committed, then a typed status in trailers.
fn streamed_refusal(form: Form, answered: &Streamed) -> Row {
    match &answered.messages {
        Ok(messages) => Err(format!(
            "the refused stream ended cleanly after {messages:?}"
        )),
        Err(status) => all([
            expect_eq(
                "echoed head metadata before the refusal",
                metadata_value(&answered.head, ECHO_KEY),
                Some(form.name()),
            ),
            refusal_checks(form, status),
        ]),
    }
}

/// The messages a refused stream produced before its status.
async fn opened_before_refusal(
    response: StreamedOutcome,
    expected_first: &str,
) -> Result<Streamed, String> {
    let (head, mut stream) = opened(response)?;
    let first = stream
        .message()
        .await
        .map_err(|status| format!("the stream refused before its first message: {status:?}"))?
        .map(|reply| reply.message);
    match first.as_deref() == Some(expected_first) {
        true => Ok(Streamed {
            head,
            messages: drained(&mut stream).await,
            trailers: Ok(None),
        }),
        false => Err(format!(
            "the refused stream opened with {first:?}, not {expected_first:?}"
        )),
    }
}

fn server_streaming_success_keeps_metadata_and_trailer(fixture: &FormsFixture) -> Row {
    let form = Form::ServerStreaming;
    entered_once(fixture, form, || {
        let answered = bounded("the server-streaming call", BOUND, async {
            let mut client = fixture.client().await;
            streamed(
                client
                    .server_streaming(credentialed(form, hello("stream")))
                    .await,
            )
            .await
        })??;
        streamed_success(form, &answered, &["Hello, stream!", "Goodbye, stream!"])
    })
}

fn server_streaming_refusal_trails_a_committed_message(fixture: &FormsFixture) -> Row {
    let form = Form::ServerStreaming;
    entered_once(fixture, form, || {
        let answered = bounded("the refused server-streaming call", BOUND, async {
            let mut client = fixture.client().await;
            let response = client
                .server_streaming(credentialed(form, hello(FAIL_NAME)))
                .await;
            opened_before_refusal(response, "Hello, fail!").await
        })??;
        streamed_refusal(form, &answered)
    })
}

fn bidirectional_success_keeps_metadata_and_trailer(fixture: &FormsFixture) -> Row {
    let form = Form::Bidirectional;
    entered_once(fixture, form, || {
        let answered = bounded("the bidirectional call", BOUND, async {
            let mut client = fixture.client().await;
            let response = client
                .bidirectional(credentialed(form, hellos(&["first", "second"])))
                .await;
            streamed(response).await
        })??;
        streamed_success(form, &answered, &["Hello, first!", "Hello, second!"])
    })
}

fn bidirectional_refusal_trails_a_committed_message(fixture: &FormsFixture) -> Row {
    let form = Form::Bidirectional;
    entered_once(fixture, form, || {
        let answered = bounded("the refused bidirectional call", BOUND, async {
            let mut client = fixture.client().await;
            let response = client
                .bidirectional(credentialed(form, hellos(&["first", FAIL_NAME])))
                .await;
            opened_before_refusal(response, "Hello, first!").await
        })??;
        streamed_refusal(form, &answered)
    })
}

/// An unregistered service: `UNIMPLEMENTED` on a `200`, recorded once.
fn unknown_service_is_unimplemented_and_recorded_once(fixture: &FormsFixture) -> Row {
    let expected = Expected::of("unknown service", "POST", "grpc").from("grpc");
    let addr = fixture.server.addr();
    let capture = common::capture_events(&format!("path={UNKNOWN_PATH}"));
    let before = Recorded::scraped(addr);
    let mut headers = GRPC_HEADERS.to_vec();
    headers.push((AUTH_HEADER, AUTH_VALUE));
    let answered = bounded("the unknown-service call", BOUND, async {
        let mut client = common::PersistentH2Client::connect(addr, BOUND).await;
        let answered = client
            .send_complete(
                "POST",
                UNKNOWN_PATH,
                "localhost",
                &headers,
                &hello_frame("unknown"),
            )
            .await;
        client.close().await;
        answered
    })?;
    let settled = http_support::poll_until(BOUND, || {
        moved(&before, &Recorded::scraped(addr), &expected) >= 1
    });
    all([
        expect_eq("wire status", answered.status, 200),
        expect_eq(
            "grpc-status",
            answered.header("grpc-status"),
            Some(UNIMPLEMENTED),
        ),
        expect_eq(
            "content-type",
            answered.header("content-type"),
            Some("application/grpc"),
        ),
        expect("no completion was recorded", settled),
        expect_eq(
            "completion records",
            moved(&before, &Recorded::scraped(addr), &expected),
            1,
        ),
    ])?;
    assert_event_matches(&only_completion(&capture, expected.label), &expected);
    Ok(())
}

/// The mapper the empty records above are read against does count.
fn mapper_counts_a_refusal_it_owns(fixture: &FormsFixture) -> Row {
    let refused = http_support::send(fixture.server.addr(), "GET", UNROUTED_PATH, &[], b"");
    all([
        expect_eq("unrouted status", refused.status, 404),
        expect_eq("unrouted body", refused.text().as_ref(), "mapped"),
        expect_eq("mapper invocations", fixture.mapped(), 1),
    ])
}

/// The gRPC rows, each of which must leave the mapper untouched.
const GRPC_ROWS: [(&str, fn(&FormsFixture) -> Row); 10] = [
    ("uncredentialed calls", uncredentialed_calls_enter_no_form),
    ("unary success", unary_success_keeps_metadata_and_trailer),
    ("unary refusal", unary_refusal_keeps_its_status),
    (
        "client-streaming success",
        client_streaming_success_keeps_metadata_and_trailer,
    ),
    (
        "client-streaming refusal",
        client_streaming_refusal_keeps_its_status,
    ),
    (
        "server-streaming success",
        server_streaming_success_keeps_metadata_and_trailer,
    ),
    (
        "server-streaming refusal",
        server_streaming_refusal_trails_a_committed_message,
    ),
    (
        "bidirectional success",
        bidirectional_success_keeps_metadata_and_trailer,
    ),
    (
        "bidirectional refusal",
        bidirectional_refusal_trails_a_committed_message,
    ),
    (
        "unknown service",
        unknown_service_is_unimplemented_and_recorded_once,
    ),
];

/// Run one row, turning a panic inside it into its verdict.
fn verdict(fixture: &FormsFixture, row: fn(&FormsFixture) -> Row) -> Row {
    catch_unwind(AssertUnwindSafe(|| row(fixture))).unwrap_or_else(|panic| {
        Err(format!(
            "panicked: {}",
            http_support::panic_text(panic.as_ref())
        ))
    })
}

/// A gRPC row's verdict, plus the claim that it reached no mapper.
fn unmapped_verdict(fixture: &FormsFixture, row: fn(&FormsFixture) -> Row) -> Row {
    let before = fixture.mapped();
    let verdict = verdict(fixture, row);
    all([
        verdict,
        expect_eq("mapper invocations", fixture.mapped(), before),
    ])
}

fn assert_native_forms() {
    common::test_runtime()
        .with_metrics()
        .with_tracing()
        .shutdown_timeout(BOUND)
        .run(|| {
            let fixture = FormsFixture::serve();
            let verdicts: Box<[(&str, Row)]> = GRPC_ROWS
                .iter()
                .map(|(name, row)| (*name, unmapped_verdict(&fixture, *row)))
                .chain([(
                    "mapper calibration",
                    verdict(&fixture, mapper_counts_a_refusal_it_owns),
                )])
                .collect();
            let teardown = fixture.tear_down();
            runtime::request_shutdown();
            assert_verdicts(
                "native tonic form checks",
                verdicts.into_iter().chain([("teardown", teardown)]),
            );
        })
        .expect("the native forms runtime ran to completion");
}

/// 17.T1 — invariants 12 and 15.
///
/// The completion counter is process-global, so the matrix runs in a private
/// child where no sibling test can record a gRPC completion between scrapes.
#[test]
fn native_tonic_forms_preserve_metadata_status_and_unknown_completion() {
    common::run_in_child(
        "grpc_integrations::native_tonic_forms_preserve_metadata_status_and_unknown_completion",
        "native-tonic-forms",
        "NATIVE_TONIC_FORMS_COMPLETE",
        MATRIX_BOUND,
        assert_native_forms,
    );
}
