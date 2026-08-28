//! What a row needs to hold a real `Router::ws` callback in application code,
//! and to get a peer onto the bridge that started it.
//!
//! Both live here because both are the same claim seen from two ends. A row
//! about the retained callback needs a callback that genuinely does not answer
//! the endpoints its bridge closed, and a peer that genuinely completed the
//! upgrade handshake. A second copy of either is a second definition of what
//! "still in application code" and "upgraded" mean.

use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// The two ends of the gate a row's callback parks on.
///
/// The receiver is what the row's callback waits on through
/// [`park_until_released`](super::ws_directions::park_until_released), behind an
/// `Arc` because the router that carries it is built once and every bridge it
/// serves parks on the same one. The sender is what the row holds and never
/// sends on: dropping it is the only release there is, so a row that unwinds
/// past its last line still lets every parked callback return, and a row whose
/// claim is the moment of release drops it at that moment.
///
/// Deliberately not a Camber primitive. A callback parked on something Camber
/// has no part in is exactly the subject: closing its receive queue and its
/// send admission wakes nothing, which is what makes the row about the join
/// deadline rather than about a cooperative return.
pub fn callback_gate() -> (Sender<()>, Arc<Mutex<Receiver<()>>>) {
    let (release, parked) = channel();
    (release, Arc::new(Mutex::new(parked)))
}

/// Open a peer and take it through the handshake on `path` to its `101`.
pub async fn upgraded_ws_peer(
    addr: SocketAddr,
    path: &str,
    context: &str,
) -> tokio::net::TcpStream {
    let mut peer = tokio::net::TcpStream::connect(addr)
        .await
        .unwrap_or_else(|error| panic!("{context}: connecting the upgrade peer failed: {error}"));
    tokio::io::AsyncWriteExt::write_all(&mut peer, super::ws::ws_upgrade_request(path).as_bytes())
        .await
        .unwrap_or_else(|error| panic!("{context}: writing the handshake failed: {error}"));
    let head = super::ws_async::read_async_http_head(&mut peer, context).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "{context}: the upgrade was refused: {head}"
    );
    peer
}

/// Send the peer's close frame, which is a bridge's local terminal.
pub async fn close_ws_peer(peer: &mut tokio::net::TcpStream, context: &str) {
    super::ws_async::write_async_ws_frame(peer, super::ws_directions::CLOSE, &[], context).await;
}

/// Every record this listener's bridges have published about their callbacks.
pub fn published_callbacks(
    controller: &camber::http::mock::ScopedRetainedCallback,
) -> Box<[camber::http::mock::WebSocketCallbackObservation]> {
    controller.upgrades.callbacks()
}

/// Assert every published record names `owner` as the upgrade above it.
///
/// The parent half of the callback-ownership claim, read from two writers
/// rather than inferred from one: the connection records the transfer, the
/// bridge records the callback, and a callback beneath a different upgrade — or
/// beneath none — disagrees here. Both the component row and the daemon-live
/// acceptance row make this claim, so the sentence that states it has one home
/// and cannot drift into two spellings of the same failure.
pub fn assert_callbacks_own(
    decisions: &[camber::http::mock::WebSocketCallbackObservation],
    owner: (u64, u64),
    context: &str,
) {
    assert!(
        !decisions.is_empty(),
        "{context}: nothing was published about the callback"
    );
    for decision in decisions {
        assert_eq!(
            (decision.connection, decision.upgrade),
            owner,
            "{context}: the callback names an upgrade its connection never transferred: {decision:?}"
        );
    }
}
