//! The routes the disconnect journeys register, and the one middleware gate
//! that watches a request production owns end to end.
//!
//! Every route here arms a probe from inside a served handler: the disconnect
//! signal has no constructor outside a connection, so a route is the only way
//! in.

use super::probes::{Admission, CauseProbe, Report, Stages, staged};
#[cfg(any(feature = "grpc", feature = "ws"))]
use camber::http::Next;
use camber::http::{DisconnectSignal, Request, Response, Router, StreamResponse, StreamSender};
use camber::{AsyncJoinHandle, RuntimeError};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

/// Route every fixture probes before it starts the traffic it measures.
pub(crate) const READY_PATH: &str = "/ready";

/// A router carrying the readiness route every fixture probes.
///
/// Every case starts from this rather than from `Router::new`: the readiness
/// route is what `start_owned` waits on, so a router built without it would
/// fail startup rather than the behavior under test.
pub(crate) fn probe_router() -> Router {
    let mut router = Router::new();
    router.get(READY_PATH, |_req: &Request| async {
        Response::text(200, "ready")
    });
    router
}

/// Register a route whose handler parks forever after arming its watcher.
///
/// A parked handler never produces a response, so a cause this probe reports
/// provably resolved before any response header byte could be written.
pub(super) fn route_parked(router: &mut Router, path: &str) -> CauseProbe {
    route_parked_clones(router, path, 1)
}

/// Register a parked route whose signal is cloned `clones` times, each clone
/// awaited by its own watcher reporting to one sink.
pub(super) fn route_parked_clones(router: &mut Router, path: &str, clones: usize) -> CauseProbe {
    let (sinks, probe) = CauseProbe::pair();
    router.get(path, move |req: &Request| {
        let sinks = Arc::clone(&sinks);
        let signal = req.on_disconnect();
        let subject: Box<str> = req.path().into();
        async move {
            sinks.watch(&subject, signal, clones);
            std::future::pending::<()>().await;
            Response::text(200, "a parked handler never responds")
        }
    });
    probe
}

/// Register a route that responds immediately while its watcher keeps holding
/// the signal, so the completion transition is still reported.
pub(super) fn route_responding<R, F>(router: &mut Router, path: &str, respond: F) -> CauseProbe
where
    F: Fn() -> R + Send + Sync + 'static,
    R: camber::http::IntoResponse + Send + 'static,
{
    let (sinks, probe) = CauseProbe::pair();
    router.get(path, move |req: &Request| {
        let sinks = Arc::clone(&sinks);
        let signal = req.on_disconnect();
        let subject: Box<str> = req.path().into();
        let response = respond();
        async move {
            sinks.watch(&subject, signal, 1);
            response
        }
    });
    probe
}

/// What a middleware gate does with the request it watched.
#[cfg(any(feature = "grpc", feature = "ws"))]
#[derive(Clone, Copy)]
pub(super) enum Hold {
    /// Pass the request on to its route.
    PassThrough,
    /// Keep the request in flight, so nothing can produce a response for it.
    InFlight,
}

/// Watch one route's signal from production middleware.
///
/// The gate runs before the transport handoffs that establish `Completed` for
/// an upgraded or tonic-owned response, so a watcher armed here survives the
/// service future Hyper drops — which is what makes those handoffs observable
/// at all. The gate must never await the signal itself: it would deadlock on
/// the handoff it has to pass to reach.
///
/// `path` is matched exactly, and naming one path is the only filter offered.
/// The readiness sequence sends two requests before any case's own traffic — a
/// deliberately malformed `GET /` transport probe and `GET /ready` — so an
/// exclusion filter admits whichever of them it did not name, and the gate
/// would report a request that never reached the subsystem under proof.
#[cfg(any(feature = "grpc", feature = "ws"))]
pub(super) fn gate_probe(router: &mut Router, path: &'static str, hold: Hold) -> CauseProbe {
    let (sinks, probe) = CauseProbe::pair();
    router.use_middleware(move |req: &Request, next: Next| {
        let sinks = Arc::clone(&sinks);
        let signal = req.on_disconnect();
        let subject: Box<str> = req.path().into();
        let watched = &*subject == path;
        let downstream = next.call(req);
        async move {
            match (watched, hold) {
                (false, _) => downstream.await,
                (true, Hold::PassThrough) => {
                    sinks.watch(&subject, signal, 1);
                    downstream.await
                }
                (true, Hold::InFlight) => {
                    sinks.watch(&subject, signal, 1);
                    std::future::pending::<Response>().await
                }
            }
        }
    });
    probe
}

/// Register a streaming route whose spawned producer is `produce`.
///
/// Every streaming probe here has one shape: produce the head, arm `clones`
/// watchers over the response's signal, then hand the sender and a clone of
/// that signal to a producer spawned through the root scope. Only the producer
/// body differs, so only the producer body is written per case.
pub(super) fn route_stream<P, F>(
    router: &mut Router,
    path: &str,
    clones: usize,
    produce: P,
) -> (CauseProbe, Admission)
where
    P: Fn(StreamSender, DisconnectSignal) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    register_stream(router, path, clones, None, produce)
}

/// Register a streaming route whose response declares a `content-length`.
///
/// Hyper picks its HTTP/1 encoder from the headers rather than from the body's
/// size hint, so a declared length is a different production path than a
/// chunked stream: the encoder can finish the body without a final poll. That
/// is the ordinary streaming-proxy shape, and it has its own completion point.
pub(super) fn route_sized_stream<P, F>(
    router: &mut Router,
    path: &str,
    length: usize,
    produce: P,
) -> (CauseProbe, Admission)
where
    P: Fn(StreamSender, DisconnectSignal) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    register_stream(router, path, 1, Some(length), produce)
}

/// The one streaming-route registration both public forms delegate to.
fn register_stream<P, F>(
    router: &mut Router,
    path: &str,
    clones: usize,
    declared_length: Option<usize>,
    produce: P,
) -> (CauseProbe, Admission)
where
    P: Fn(StreamSender, DisconnectSignal) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let (sinks, probe) = CauseProbe::pair();
    let (admitted, admission) = Admission::pair();
    router.get_stream(path, move |req: &Request| {
        let sinks = Arc::clone(&sinks);
        let admitted = Arc::clone(&admitted);
        let produce = produce.clone();
        let signal = req.on_disconnect();
        let subject: Box<str> = req.path().into();
        Box::pin(async move {
            let (response, sender) = declared_response(declared_length);
            sinks.watch(&subject, signal.clone(), clones);
            let producer = camber::spawn_async(produce(sender, signal));
            report_admission(producer, &admitted).await;
            response
        })
    });
    (probe, admission)
}

/// Build the streaming response, declaring `length` when the case asked for it.
fn declared_response(length: Option<usize>) -> (StreamResponse, StreamSender) {
    let (response, sender) = StreamResponse::new(200);
    match length {
        Some(length) => (
            response.with_header("content-length", &length.to_string()),
            sender,
        ),
        None => (response, sender),
    }
}

/// Report whether the root scope admitted the producer, without waiting for it.
///
/// A refused spawn carries its error already terminal and yields it on the
/// first poll; an admitted one stays pending until the producer finishes. One
/// biased poll against a ready future therefore separates the two answers and
/// never waits on the producer — which every case here needs to outlive the
/// handler.
async fn report_admission(producer: AsyncJoinHandle<()>, admitted: &Report<Option<RuntimeError>>) {
    use std::future::IntoFuture;

    let polled_once = std::future::ready(());
    tokio::select! {
        biased;
        outcome = producer.into_future() => admitted.send(outcome.err()),
        () = polled_once => admitted.send(None),
    }
}

/// Register a streaming route whose producer is released stage by stage from
/// the test thread.
///
/// The take-once shim is identical in every staged case — the receiver can be
/// handed to exactly one request, which is one more than these routes are asked
/// to serve — so it lives here and each case supplies only its producer body.
pub(super) fn route_staged_stream<P, F>(
    router: &mut Router,
    path: &str,
    clones: usize,
    produce: P,
) -> (CauseProbe, Admission, Stages)
where
    P: Fn(UnboundedReceiver<()>, StreamSender) -> F + Clone + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let (receiver, stages) = staged();
    let (probe, admission) = route_stream(router, path, clones, move |sender, _| {
        let stage = receiver
            .take()
            .expect("a staged streaming route was requested more than once");
        produce(stage, sender)
    });
    (probe, admission, stages)
}
