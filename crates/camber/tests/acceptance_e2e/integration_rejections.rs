//! A typed integration failure returned from a real handler, read off the wire.
//!
//! The claim is about the HTTP boundary a service answers on: an integration
//! error travels through the existing internal-service rejection, the mapper is
//! handed only client-safe context, and the peer reads the fixed redacted body.
//! The source the adapter attached stays out of both. Every value asserted here
//! was read from a socket, the mapper journal, or the captured operator events.

use crate::common;
use crate::http as wire;
use crate::leaky_source::LeakySource;

use camber::http::{Next, Rejection, RejectionContext, Request, Response, Router};
use camber::runtime;
use camber::runtime_test_support::IntegrationErrorDriver;
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError,
};
use common::{Journal, assert_field_value, assert_fields, only_event, recording_mapper};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The route whose handler returns the integration failure.
const INTEGRATION_PATH: &str = "/handler-integration-failure";

/// The same route behind the refusing middleware. Each row captures events by
/// its own path, so rows running in parallel never read each other's events.
const MIDDLEWARE_PATH: &str = "/middleware-integration-failure";

/// The path the row with `middleware` serves and captures under.
const fn row_path(middleware: bool) -> &'static str {
    match middleware {
        true => MIDDLEWARE_PATH,
        false => INTEGRATION_PATH,
    }
}

/// The origin the recording mapper files its observations under.
const POLICY: &str = "integration";

/// What the adapter's source says, which no peer or mapper may read.
const SOURCE_TEXT: &str = "sqs send failed at queue-sentinel.local.test";

/// A secret the source echoes from the request it failed.
const SOURCE_SECRET: &str = "receipt-sentinel-AQEBx9";

/// The third-party failure the handler's integration error carries.
fn adapter_failure() -> LeakySource {
    LeakySource::echoing(format!("{SOURCE_TEXT}: receipt={SOURCE_SECRET}"))
}

/// The integration failure one handler invocation returns.
///
/// Built through the production factory each time, so the value the server
/// maps is the one an adapter would have produced, source and all.
fn integration_failure() -> RuntimeError {
    let error = IntegrationErrorDriver::new(
        IntegrationKind::Sqs,
        IntegrationOperation::Publish,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
    .instance(1)
    .source(Arc::new(adapter_failure()))
    .build();
    RuntimeError::Integration(Arc::new(error))
}

/// Serve the one failing route, counting how often its handler ran.
fn integration_server(
    journal: &Journal,
    handled: &Arc<AtomicUsize>,
    middleware: bool,
) -> wire::ReadyServer {
    let mut router = Router::new();
    if middleware {
        router.use_middleware(|_req: &Request, _next: Next| async {
            Err::<Response, RuntimeError>(integration_failure())
        });
    }
    let calls = Arc::clone(handled);
    router.get(row_path(middleware), move |_req: &Request| {
        calls.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Err::<Response, RuntimeError>(integration_failure()))
    });
    let recorder = recording_mapper(journal, POLICY);
    let router =
        router.rejection_mapper(move |rejection: &Rejection, context: &RejectionContext| {
            common::naming(recorder(rejection, context), context)
        });
    common::block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the fixture listener binds an ephemeral loopback port");
        wire::serve_owned(listener, move |listener| {
            camber::http::serve_background(listener, router)
                .expect("owned server requires a Tokio runtime")
        })
    })
    .expect("the owned server reports its bound address")
}

/// Everything private the failure carried, in every form it could leak as.
///
/// The source is private whole and in parts, and the integration error's own
/// operator rendering is private too: the peer and the mapper read only the
/// fixed internal-service text.
fn private_text() -> [String; 4] {
    [
        SOURCE_TEXT.to_owned(),
        SOURCE_SECRET.to_owned(),
        adapter_failure().to_string(),
        integration_failure().to_string(),
    ]
}

/// Assert the operator saw one internal-service refusal and one completion.
fn assert_one_outcome(captured: &common::TraceCapture, path: &str, request_id: &str) {
    let events = captured.events();
    let identity = format!("request_id={request_id}");
    let label = "integration outcome";
    let target = format!("raw_path={path}");
    let rejected = only_event(&events, common::REJECTION_MESSAGE, label);
    assert_field_value(rejected, "status", "500", label);
    assert_fields(
        rejected,
        &[&identity, "kind=internal_service", &target],
        label,
    );
    let completed = only_event(&events, common::COMPLETION_MESSAGE, label);
    assert_field_value(completed, "status", "500", label);
    assert_fields(completed, &[&identity], label);
}

#[test]
fn integration_failure_returns_one_redacted_internal_response() {
    assert_redacted_integration_response(false);
}

#[test]
fn middleware_integration_failure_uses_internal_service_rejection() {
    assert_redacted_integration_response(true);
}

fn assert_redacted_integration_response(middleware: bool) {
    let path = row_path(middleware);
    let captured = common::capture_events(path);

    common::test_runtime()
        .with_tracing()
        .run(|| {
            let journal = common::journal();
            let handled = Arc::new(AtomicUsize::new(0));
            let served = integration_server(&journal, &handled, middleware);

            let response = wire::send(served.local_addr(), "GET", path, &[], b"");
            assert_eq!(response.status, 500, "the redacted wire status");
            assert_eq!(
                response.text().as_ref(),
                common::REDACTED_BODY,
                "the redacted wire body"
            );

            let private = private_text();
            common::assert_no_private_text(&response, &private, "integration peer");

            assert_eq!(
                handled.load(Ordering::SeqCst),
                usize::from(!middleware),
                "middleware refusal must precede the handler"
            );
            let mapped = common::assert_mapped_internal_refusal(
                &journal,
                POLICY,
                &private,
                "integration mapper",
            );
            assert_eq!(mapped.raw_path.as_ref(), path, "integration mapper: path");

            let request_id = common::request_id_of(&response, "integration peer");
            assert_one_outcome(&captured, path, &request_id);

            served
                .shutdown_bounded(wire::WIRE_TIMEOUT)
                .expect("the owned server stops and joins within its bound");
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
