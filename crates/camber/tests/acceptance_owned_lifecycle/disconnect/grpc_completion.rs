//! gRPC: Camber owns the body around tonic's answer, so `Completed` is
//! established where every other response establishes it — at the body's own
//! terminal, once tonic's last frame and its trailers have been produced.
//!
//! Observed through the production middleware gate, which arms a watcher that
//! outlives the service future Hyper drops. The RPC is parked inside tonic's
//! handler first, so the watcher is armed while tonic still owes every byte;
//! releasing it and reading the reply is what makes the resolved cause the
//! body's own, and not a transition taken before the answer existed.

use super::fixture::bounded;
use super::probes::{EntryBarrier, Report, StageReceiver, Stages, entry_barrier, staged};
use super::routes::{Hold, gate_probe, probe_router};
use super::servers::with_owned_server;
use camber::http::{DisconnectCause, GrpcRouter};
use std::net::SocketAddr;
use std::sync::Arc;

mod proto {
    tonic::include_proto!("greeter");
}

use proto::greeter_service;

/// The wire path tonic dispatches `SayHello` on.
///
/// The gate watches this and nothing else. Readiness sends two requests before
/// the RPC — a deliberately malformed `GET /` transport probe and `GET /ready`
/// — so a gate that only excluded the readiness route would have the probe
/// inside its filter, and a 404 that never touched tonic could be reported as
/// the RPC's own completion.
const RPC_PATH: &str = "/greeter.Greeter/SayHello";

/// A greeter that reports entry and then parks until the test releases it.
///
/// Parking is what makes the case discriminating: while the handler waits,
/// tonic has produced no frame and no trailer, so a resolved signal can only
/// have come from the handoff transition. Every way the park could silently not
/// happen is a panic here rather than a quietly weaker test.
struct ParkedGreeter {
    entered: Arc<Report<()>>,
    release: StageReceiver,
}

/// The test-side half: waits for the RPC to be parked, then releases it.
struct RpcPark {
    entered: EntryBarrier,
    release: Stages,
}

impl ParkedGreeter {
    fn pair() -> (Self, RpcPark) {
        let (entered, entries) = entry_barrier();
        let (release, stages) = staged();
        (
            Self { entered, release },
            RpcPark {
                entered: entries,
                release: stages,
            },
        )
    }
}

impl RpcPark {
    /// Block until tonic's handler is running and owes a response.
    fn await_entry(&self) {
        self.entered.await_entry();
    }

    fn release(&self) {
        self.release.advance();
    }
}

#[tonic::async_trait]
impl greeter_service::Greeter for ParkedGreeter {
    async fn say_hello(
        &self,
        request: tonic::Request<proto::HelloRequest>,
    ) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
        // Taken before entry is reported, so the test's release cannot land
        // before there is anything holding it. The lock is not held across the
        // await.
        let mut release = self
            .release
            .take()
            .expect("the parked greeter was called more than once");
        self.entered.send(());
        release
            .recv()
            .await
            .expect("the parked RPC's release channel was dropped before the test released it");
        let name = request.into_inner().name;
        Ok(tonic::Response::new(proto::HelloReply {
            message: format!("Hello, {name}!"),
        }))
    }
}

async fn say_hello(addr: SocketAddr, name: &str) -> Box<str> {
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("failed to build the gRPC endpoint")
        .connect()
        .await
        .expect("failed to connect the gRPC channel");
    let mut client = proto::greeter_client::GreeterClient::new(channel);
    client
        .say_hello(tonic::Request::new(proto::HelloRequest {
            name: name.into(),
        }))
        .await
        .expect("the gRPC call failed")
        .into_inner()
        .message
        .into_boxed_str()
}

/// What the parked RPC measured.
struct GrpcOutcome {
    /// The path of the request the gate actually watched.
    watched: Box<str>,
    cause: DisconnectCause,
    message: Box<str>,
}

#[test]
fn grpc_request_signal_resolves_completed_via_middleware_gate() {
    let mut router = probe_router();
    let gate = gate_probe(&mut router, RPC_PATH, Hold::PassThrough);
    let (greeter, park) = ParkedGreeter::pair();
    router.grpc(GrpcRouter::new().add_service(greeter_service::serve(greeter)));

    let observed = with_owned_server(router, |addr| {
        let call = camber::spawn_async(say_hello(addr, "Camber"));

        // The entry barrier is the proof that the RPC is outstanding, so no
        // timer re-proves it: `await_entry` returns only once tonic's handler
        // has taken the stage receiver, and nothing advances that stage until
        // the release below. The gate's watcher is therefore armed while tonic
        // still owes every frame and every trailer.
        park.await_entry();

        park.release();
        let message =
            bounded("the gRPC call to return", call).expect("the gRPC call task did not return");
        // Read after the answer, because Camber's own body is what resolves it:
        // the guard lives in the response body around tonic's, and that body
        // completes on the trailer set tonic wrote.
        let (watched, cause) = gate.watched_cause();
        GrpcOutcome {
            watched,
            cause,
            message,
        }
    });

    assert_eq!(
        observed.watched.as_ref(),
        RPC_PATH,
        "the gate reported a request that was not the RPC under proof"
    );
    assert_eq!(observed.message.as_ref(), "Hello, Camber!");
    assert_eq!(
        observed.cause,
        DisconnectCause::Completed,
        "a gRPC response Camber produced in full did not resolve Completed at its body's terminal"
    );
}
