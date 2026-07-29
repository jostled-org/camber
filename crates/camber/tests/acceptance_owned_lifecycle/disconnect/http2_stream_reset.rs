//! HTTP/2: `RST_STREAM` ends one stream's response lifetime and nothing else.
//!
//! The reset is SENT by the dev-only `h2` client. Camber never reads it —
//! Hyper drops the per-request future and Camber observes its own guard, which
//! is why no production `h2` dependency exists.

use super::fixture::bounded;
use super::routes::{READY_PATH, probe_router, route_parked};
use super::servers::with_owned_server;
use camber::http::DisconnectCause;
use std::net::SocketAddr;

/// One HTTP/2 connection whose driver task this fixture owns.
///
/// The address is stored at `open`: a session that is already connected cannot
/// be asked which peer it belongs to on every request without inviting two
/// answers.
struct H2Session {
    addr: SocketAddr,
    client: Option<h2::client::SendRequest<bytes::Bytes>>,
    driver: Option<tokio::task::JoinHandle<Result<(), h2::Error>>>,
}

impl H2Session {
    /// Open one HTTP/2 connection to `addr`.
    ///
    /// The bound belongs to the session rather than to each test, for the same
    /// reason `peer::read_head` and `servers::await_lifecycle_pause` own theirs:
    /// every case wants the same one, and a bound restated per call site is a
    /// bound one call site can forget.
    fn open(addr: SocketAddr) -> Self {
        bounded("the HTTP/2 session to open", Self::connect(addr))
    }

    /// The handshake [`H2Session::open`] bounds.
    async fn connect(addr: SocketAddr) -> Self {
        let tcp = tokio::net::TcpStream::connect(addr)
            .await
            .expect("failed to connect the HTTP/2 peer");
        let (client, connection) = h2::client::handshake(tcp)
            .await
            .expect("the HTTP/2 handshake failed");
        Self {
            addr,
            client: Some(client),
            driver: Some(tokio::spawn(connection)),
        }
    }

    /// Start a request and keep both halves, so nothing is reset by a drop.
    ///
    /// `ready` first: `h2` documents it as `send_request`'s precondition. It
    /// consumes and returns the handle, so a clone of the session's own handle
    /// is what waits — the connection is shared behind both.
    async fn start(
        &mut self,
        path: &str,
    ) -> (h2::client::ResponseFuture, h2::SendStream<bytes::Bytes>) {
        let request = ::http::Request::get(format!("http://{}{}", self.addr, path))
            .body(())
            .expect("failed to build the HTTP/2 request");
        let mut client = self
            .client
            .as_ref()
            .expect("the HTTP/2 session was already closed")
            .clone()
            .ready()
            .await
            .expect("the HTTP/2 client never became ready to send");
        client
            .send_request(request, true)
            .expect("failed to open the HTTP/2 stream")
    }

    /// Run one request to completion on this connection and return its status.
    fn status(&mut self, path: &str) -> u16 {
        bounded("the HTTP/2 request to complete", self.complete(path))
    }

    /// The request [`H2Session::status`] bounds.
    async fn complete(&mut self, path: &str) -> u16 {
        let (response, send) = self.start(path).await;
        let response = response.await.expect("the HTTP/2 response failed");
        let status = response.status().as_u16();
        let mut body = response.into_body();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.expect("the HTTP/2 body failed");
            body.flow_control()
                .release_capacity(chunk.len())
                .expect("failed to release HTTP/2 flow control");
        }
        // Held until the response is fully read: dropping the send half early
        // resets a stream this case needs to finish normally.
        drop(send);
        status
    }

    /// End the connection and join its driver task.
    fn close(&mut self) {
        bounded("the HTTP/2 session to close", self.shutdown());
    }

    /// The teardown [`H2Session::close`] bounds.
    async fn shutdown(&mut self) {
        drop(self.client.take());
        let driver = match self.driver.take() {
            Some(driver) => driver,
            None => panic!("the HTTP/2 session was already closed"),
        };
        driver.abort();
        match driver.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("the HTTP/2 client driver failed: {error}"),
            Err(error) if error.is_cancelled() => {}
            Err(error) => panic!("the HTTP/2 client driver join failed: {error}"),
        }
    }
}

impl Drop for H2Session {
    /// A safety net, not the shutdown path: `close` is only reached when every
    /// assertion before it held, so a failing case would otherwise leave the
    /// driver task detached for the rest of the binary's run.
    fn drop(&mut self) {
        match self.driver.take() {
            Some(driver) => driver.abort(),
            None => {}
        }
    }
}

#[test]
fn live_connection_single_request_drop_resolves_stream_reset() {
    let mut router = probe_router();
    let reset = route_parked(&mut router, "/reset");

    let (cause, reused_status) = with_owned_server(router, |addr| {
        let mut session = H2Session::open(addr);
        let (response, mut send) = bounded("the HTTP/2 request to start", session.start("/reset"));
        reset.await_entry();

        // The connection is untouched: only this stream ends.
        send.send_reset(h2::Reason::CANCEL);
        drop(response);
        let cause = reset.cause();

        // Serving a second request on the same connection is what proves the
        // transport never reported termination — without that, the cause would
        // collapse into PeerDisconnect.
        let reused_status = session.status(READY_PATH);
        session.close();
        (cause, reused_status)
    });

    assert_eq!(
        cause,
        DisconnectCause::StreamReset,
        "a reset stream on a live connection did not resolve StreamReset"
    );
    assert_eq!(
        reused_status, 200,
        "the connection did not survive the stream reset"
    );
}

#[test]
fn http2_stream_reset_leaves_sibling_stream_signal_unresolved() {
    let mut router = probe_router();
    let reset = route_parked(&mut router, "/reset");
    let sibling = route_parked(&mut router, "/sibling");

    let (cause, sibling_open, sibling_cause) = with_owned_server(router, |addr| {
        let mut session = H2Session::open(addr);
        let (reset_response, mut reset_send) = bounded(
            "the HTTP/2 /reset request to start",
            session.start("/reset"),
        );
        let (sibling_response, sibling_send) = bounded(
            "the HTTP/2 /sibling request to start",
            session.start("/sibling"),
        );
        reset.await_entry();
        sibling.await_entry();

        reset_send.send_reset(h2::Reason::CANCEL);
        drop(reset_response);
        let cause = reset.cause();
        let sibling_open = sibling.still_unresolved();

        // The quiet window above is silence, and silence is worth only as much
        // as the receiver's liveness. Ending the sibling's own stream on the
        // same still-live connection is the positive observation over the same
        // probe: it resolves, so the window measured an unresolved signal.
        drop(sibling_response);
        drop(sibling_send);
        let sibling_cause = sibling.cause();
        session.close();
        (cause, sibling_open, sibling_cause)
    });

    assert_eq!(
        cause,
        DisconnectCause::StreamReset,
        "the reset stream did not resolve StreamReset"
    );
    assert!(
        sibling_open,
        "resetting one HTTP/2 stream resolved a concurrent stream's signal"
    );
    assert_eq!(
        sibling_cause,
        DisconnectCause::StreamReset,
        "the sibling stream never resolved when it was ended in turn, so the \
         quiet window above proves nothing"
    );
}
