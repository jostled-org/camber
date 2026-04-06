#[cfg(all(feature = "jemalloc", feature = "mimalloc"))]
compile_error!("Features \"jemalloc\" and \"mimalloc\" are mutually exclusive. Enable only one.");

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc_crate::MiMalloc = mimalloc_crate::MiMalloc;

#[cfg(feature = "acme")]
pub mod acme;
pub mod channel;
pub mod circuit_breaker;
pub mod config;
#[cfg(feature = "dns01")]
pub mod dns01;
pub mod error;
pub mod http;
pub mod logging;
#[cfg(any(feature = "nats", feature = "sqs"))]
pub mod mq;
pub mod net;
pub(crate) mod prng;
pub mod resource;
pub(crate) mod resource_lifecycle;
pub mod runtime;
pub(crate) mod runtime_state;
pub mod schedule;
pub mod secret;
mod select;
pub mod signals;
pub mod task;
pub mod time;
pub mod tls;

#[cfg(feature = "acme")]
pub use acme::AcmeConfig;
pub use camber_macros::test;
pub use error::RuntimeError;
pub use resource::Resource;
pub use runtime::RuntimeBuilder;
pub use runtime::on_cancel;
pub use task::{AsyncJoinHandle, JoinHandle, on_shutdown, race, race_all, spawn, spawn_async};
pub use time::timeout;
pub use tls::CertStore;
pub use tracing;

/// Internal re-exports for macro hygiene. Not part of the public API.
#[doc(hidden)]
pub mod __private;
