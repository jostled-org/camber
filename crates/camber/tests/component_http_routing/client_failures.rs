//! 1.T1: every outbound failure a caller meets is a closed category, and no
//! rendering of it repeats what the request carried.
//!
//! Each row sends an existing public verb at a local peer that fails in one
//! way: a port that refuses, a head that never arrives, a body cut short, a
//! certificate nothing trusts, and an answer above its ceiling. The request
//! target and payload carry sentinels. The returned error is then printed
//! every way an error can be printed, sources included, and none of those
//! renderings may carry a sentinel or the peer's address.

use crate::bounded_client::{
    ADMITTED, CEILING, CHUNKED_HEAD, CROSSING, DECLARED_HEAD, DECLARED_LENGTH, assert_client_limit,
    chunk_frame, collect_refused, within_bound,
};
use crate::http::refused_loopback_addr;
use crate::retry_upstream::{
    Answer, Call, PeerEvent, RunnableDriver, ScriptedUpstream, TIMER_ROUNDING, assert_released,
    describe, failure, settled, within_watchdog,
};
use crate::untrusted_tls::{TlsPeerEvent, untrusted_tls_peer};

use camber::RuntimeError;
use camber::http;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use std::net::SocketAddr;
use std::time::Duration;

/// The path segment every row's request target carries.
const PATH_SECRET: &str = "path-sentinel-7c1e";

/// The query value every row's request target carries.
const QUERY_SECRET: &str = "query-sentinel-41bd";

/// The payload every row that sends a body carries.
const BODY_SECRET: &str = "body-sentinel-9f3a";

/// The quiet interval of the stalled row.
///
/// Below the default attempt and operation bounds, so the paused clock crosses
/// only this transport timer.
const STALLED_IDLE: Duration = Duration::from_secs(20);

/// The path and query that carry both URL sentinels.
fn secret_target() -> String {
    format!("/{PATH_SECRET}?token={QUERY_SECRET}")
}

/// A request target on `addr` that carries both URL sentinels.
fn secret_url(scheme: &str, addr: SocketAddr) -> String {
    format!("{scheme}://{addr}{}", secret_target())
}

/// Everything one row's request carried that its error must not repeat.
///
/// The peer's address is here with the sentinels: an error that names where it
/// dialed has kept part of the URL.
fn secrets(addr: SocketAddr) -> [String; 4] {
    [
        PATH_SECRET.to_owned(),
        QUERY_SECRET.to_owned(),
        BODY_SECRET.to_owned(),
        addr.to_string(),
    ]
}

/// Require one returned error to be safe in every rendering and every source.
fn assert_safe(row: &str, error: &RuntimeError, addr: SocketAddr) {
    crate::error_chain::assert_safe(error, &secrets(addr), row);
}

/// A refused dial is a connect failure, from the free verb and the reusable
/// client alike.
async fn refused_dial_is_a_connect_failure() {
    let addr = refused_loopback_addr();
    let url = secret_url("http", addr);

    let free: Result<ClientResponse, RuntimeError> =
        within_bound("refused dial, free verb", http::post(&url, BODY_SECRET)).await;
    let reusable: Result<ClientResponse, RuntimeError> = within_bound(
        "refused dial, reusable client",
        http::client().post(&url, BODY_SECRET),
    )
    .await;

    for (row, result) in [
        ("refused dial, free verb", free),
        ("refused dial, reusable client", reusable),
    ] {
        let error = failure(row, result);
        assert!(
            matches!(error, RuntimeError::Client(ClientError::Connect)),
            "{row}: returned {error:?} instead of the connect failure"
        );
        assert_safe(row, &error, addr);
    }
}

/// A body cut short of its declared length is a transport failure.
async fn truncated_body_is_a_transport_failure() {
    let row = "truncated body";
    let mut upstream = ScriptedUpstream::bind(&[Answer::Truncate]).await;
    let addr = upstream.addr();

    let result: Result<ClientResponse, RuntimeError> =
        within_bound(row, http::post(&secret_url("http", addr), BODY_SECRET)).await;
    let started = upstream.saw(PeerEvent::Started(0)).await;
    let cut = upstream.saw(PeerEvent::Truncated(0)).await;
    // The row runs on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), row).await;

    assert_eq!(started, Ok(()), "{row}: the peer never read the request");
    assert_eq!(cut, Ok(()), "{row}: the peer never cut its answer short");
    let error = failure(row, result);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Transport)),
        "{row}: returned {error:?} instead of the transport failure"
    );
    assert_safe(row, &error, addr);
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
}

/// An untrusted certificate is a TLS failure, and the peer receives no request.
async fn untrusted_certificate_is_a_tls_failure() {
    let row = "untrusted certificate";
    let mut peer = untrusted_tls_peer().await;
    let addr = peer.addr();

    let result: Result<ClientResponse, RuntimeError> =
        within_bound(row, http::post(&secret_url("https", addr), BODY_SECRET)).await;
    let seen = within_watchdog(peer.recv()).await;
    peer.finish(row, within_watchdog).await;

    assert_eq!(
        seen,
        Some(Some(TlsPeerEvent::HandshakeRefused)),
        "{row}: the client must refuse the handshake before it sends a request"
    );
    let error = failure(row, result);
    assert!(
        matches!(error, RuntimeError::Client(ClientError::Tls)),
        "{row}: returned {error:?} instead of the TLS failure"
    );
    assert_safe(row, &error, addr);
}

/// Require the ceiling refusal, safe in every rendering.
fn assert_safe_limit(row: &str, result: Result<ClientResponse, RuntimeError>, addr: SocketAddr) {
    let error = failure(row, result);
    assert_safe(row, &error, addr);
    assert_client_limit(row, CEILING, Err(error));
}

/// A declared length above the ceiling is refused before a frame is polled.
async fn declared_oversize_is_a_response_limit_failure() {
    let row = "declared oversize";
    let body = vec![vec![b'x'; DECLARED_LENGTH].into_boxed_slice()].into_boxed_slice();
    let (result, collected, addr) =
        collect_refused(row, &secret_target(), DECLARED_HEAD, body, false).await;

    assert_safe_limit(row, result, addr);
    assert_eq!(
        collected.chunks_polled, 0,
        "{row}: a declaration must be refused before any frame is read"
    );
    assert_eq!(
        collected.peak_retained, 0,
        "{row}: nothing may be retained for a refused declaration"
    );
}

/// The frame that crosses the ceiling is dropped and ends the read.
async fn crossing_frame_is_a_response_limit_failure() {
    let row = "crossing frame";
    let body = vec![chunk_frame(ADMITTED), chunk_frame(CROSSING)].into_boxed_slice();
    let (result, collected, addr) =
        collect_refused(row, &secret_target(), CHUNKED_HEAD, body, true).await;

    assert_safe_limit(row, result, addr);
    assert!(
        collected.chunks_polled > 0,
        "{row}: the crossing frame must be read before it is refused"
    );
    assert_eq!(
        collected.peak_retained, CEILING,
        "{row}: the crossing frame must add nothing to what was retained"
    );
}

/// A head that never arrives ends at the transport's quiet interval as a
/// transport timeout, and the client closes the stalled transport.
///
/// Pause before the request arms its timer. After the peer acknowledges the
/// stall, advance past the rounded deadline and await the client's result.
async fn stalled_head_is_a_transport_timeout() {
    let row = "stalled head";
    tokio::time::pause();
    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallHead]).await;
    let addr = upstream.addr();
    let url = secret_url("http", addr);
    let call: Call = tokio::spawn(async move {
        http::client()
            .response_idle_timeout(STALLED_IDLE)
            .post(&url, BODY_SECRET)
            .await
    });

    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], row)
        .await;
    assert!(
        !call.is_finished(),
        "{row}: the call ended before its deadline"
    );
    tokio::time::advance(STALLED_IDLE + TIMER_ROUNDING).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let starts = upstream.finish(driver, row).await;
    tokio::time::resume();

    let error: RuntimeError = match result {
        Some(Err(error)) => error,
        other => panic!("{row}: returned {}", describe(&other)),
    };
    assert!(
        matches!(
            error,
            RuntimeError::Client(ClientError::Timeout(ClientTimeout::Transport))
        ),
        "{row}: returned {error:?} instead of the transport timeout"
    );
    assert_safe(row, &error, addr);
    assert_released(released, row);
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
}

/// 1.T1
#[tokio::test]
async fn outbound_failures_are_typed_and_secret_free() {
    refused_dial_is_a_connect_failure().await;
    truncated_body_is_a_transport_failure().await;
    untrusted_certificate_is_a_tls_failure().await;
    declared_oversize_is_a_response_limit_failure().await;
    crossing_frame_is_a_response_limit_failure().await;
    // Last, because it is the one row that holds the clock still.
    stalled_head_is_a_transport_timeout().await;
}
