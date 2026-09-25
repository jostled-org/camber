//! The two WebSocket bridges, and everything they need to become one.
//!
//! `handshake`, `origin` and `authority` decide whether a WebSocket may exist;
//! `backend` settles a proxied one with its backend before the peer is told;
//! `handoff` carries what a valid one earned; `ownership` decides who owns the
//! connection that results and when its `101` becomes real; `framing` is the
//! transport substrate both bridges write frames over; `callback` disposes of
//! the blocking application child only a direct bridge has. `direct` and `proxy`
//! then own two different lifecycles on top of that shared base and share
//! nothing else: a direct connection has application queues, a receive owner,
//! and one terminal cause an application reads, and a proxied one has a second
//! WebSocket.

mod authority;
mod backend;
mod callback;
mod direct;
mod framing;
mod handoff;
mod handshake;
mod origin;
mod ownership;
mod proxy;

pub use backend::{BackendWs, BackendWsConnection, backend_ws_handshake};
pub(super) use direct::{WsDirection, handle_ws_upgrade};
pub(super) use handoff::WsRefusal;
pub(super) use handshake::{
    WsUpgrade, extract_ws_upgrade, is_ws_upgrade_head, is_ws_upgrade_request,
};
pub(super) use proxy::handle_proxy_ws;
