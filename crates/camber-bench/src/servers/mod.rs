mod assets;
pub mod axum_server;
pub mod camber_server;
pub mod go_server;
mod handle;
pub mod upstream;

pub(crate) use assets::STATIC_HTML;
pub use handle::ServerHandle;
pub(crate) use handle::{bind_and_spawn, bind_listener_and_send_addr, require_upstream};
