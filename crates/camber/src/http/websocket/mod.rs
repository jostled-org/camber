//! The two endpoints of one direct WebSocket, and the compatibility facade
//! over them.
//!
//! A WebSocket has two independent directions, so it has two owners: one
//! `WsReceiver`, which is unique because only one operation may take the next
//! frame, and any number of `WsSender` clones, which share one bounded outbound
//! queue. `WsConn` is the callback-facing facade that holds both.

mod facade;
mod message;
mod receiver;
mod sender;
mod terminal;

pub use facade::WsConn;
pub use message::{WsMessage, WsReceive};
pub use receiver::WsReceiver;
pub use sender::WsSender;
pub use terminal::WsCloseCause;

pub(crate) use message::{from_frame, into_frame};
pub(crate) use terminal::TerminalState;
