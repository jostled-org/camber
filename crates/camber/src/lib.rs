//! Opinionated async Rust for IO-bound services on top of Tokio.
//!
//! Camber is for the common case: HTTP services, background jobs, proxying,
//! and runtime-scoped work where you want Tokio underneath without building
//! your whole application around Tower, extractors, or a custom runtime setup.
//!
//! # Start Here
//!
//! The normal entrypoint is [`http::serve`]:
//!
//! ```rust,no_run
//! use camber::RuntimeError;
//! use camber::http::{self, Response, Router};
//!
//! fn main() -> Result<(), RuntimeError> {
//!     let mut router = Router::new();
//!     router.get("/hello", |_req| async {
//!         Response::text(200, "Hello, world!")
//!     });
//!     http::serve("0.0.0.0:8080", router)
//! }
//! ```
//!
//! Use [`runtime::builder`] when you need runtime configuration around that
//! service, such as worker counts, shutdown timeouts, connection limits, or
//! registered resources:
//!
//! ```rust,no_run
//! use camber::{RuntimeError, runtime};
//! use std::time::Duration;
//!
//! fn main() -> Result<(), RuntimeError> {
//!     runtime::builder()
//!         .worker_threads(8)
//!         .shutdown_timeout(Duration::from_secs(10))
//!         .run(|| Ok::<(), RuntimeError>(()))?
//! }
//! ```
//!
//! # Core Model
//!
//! Camber keeps the public model small:
//!
//! - [`http::Router`] registers handlers and middleware
//! - [`http::Request`] provides string-based access to params, query, headers,
//!   cookies, and body
//! - [`http::Response`] builds owned HTTP responses
//! - [`spawn`] and [`spawn_async`] run structured background work
//! - [`Resource`] integrates long-lived dependencies into startup, health
//!   checks, and shutdown
//!
//! Handlers are async closures over `&Request`:
//!
//! ```rust
//! use camber::http::{Request, Response, Router};
//!
//! let mut router = Router::new();
//! router.get("/users/:id", |req: &Request| {
//!     let user_id = match req.param("id") {
//!         Some(id) => id.to_owned(),
//!         None => String::new(),
//!     };
//!     async move {
//!         Response::text(200, &user_id)
//!     }
//! });
//! ```
//!
//! If you need request data after an `.await`, copy owned data out before the
//! `async move`. The returned future must be `Send + 'static`.
//!
//! # Main Modules
//!
//! - [`http`]: servers, router, requests, responses, middleware, client, SSE,
//!   and proxying
//! - [`runtime`]: runtime configuration, shutdown, and lifecycle entrypoints
//! - [`task`]: structured task spawning, cancellation, and shutdown waiting
//! - [`channel`]: bounded sync channels and async MPSC channels
//! - [`resource`]: resource lifecycle integration
//! - [`schedule`]: interval and cron-style scheduled work
//! - [`net`]: low-level listeners, TCP, UDP, and forwarding
//! - [`tls`]: certificate loading, TLS config, and client connections
//! - [`logging`]: tracing subscriber setup helpers
//! - [`config`]: TOML config loading and shared TLS config types
//!
//! # Error Model
//!
//! Camber uses [`RuntimeError`] at the runtime boundary. Most top-level APIs
//! return `Result<_, RuntimeError>`, so library code can use normal `?`
//! propagation without mapping between framework-specific error types.
//!
//! # Feature Flags
//!
//! Optional capabilities are feature-gated:
//!
//! - `ws`: WebSocket routes
//! - `grpc`: gRPC serving support
//! - `otel`: OpenTelemetry tracing and export
//! - `acme`: automatic TLS via ACME
//! - `dns01`: ACME DNS-01 support
//! - `nats`, `sqs`: message queue integrations
//!
//! # Choosing Camber
//!
//! Camber is a good fit when:
//!
//! - your service is IO-bound
//! - you want explicit request and response handling
//! - you want Tokio underneath but not Tokio ceremony everywhere
//! - you prefer a small built-in surface over assembling many crates up front
//!
//! If you need fine-grained executor control, Tower-heavy composition, or a
//! lower-level transport stack, use Tokio and its surrounding ecosystem
//! directly.
//!
#[cfg(all(feature = "jemalloc", feature = "mimalloc"))]
compile_error!("Features \"jemalloc\" and \"mimalloc\" are mutually exclusive. Enable only one.");

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc_crate::MiMalloc = mimalloc_crate::MiMalloc;

#[cfg(feature = "acme")]
/// Automatic TLS via ACME.
pub mod acme;
/// Sync and async channel primitives.
pub mod channel;
/// Circuit breaker wrapper for managed resources.
pub mod circuit_breaker;
/// Shared config parsing and TLS config types.
pub mod config;
#[cfg(feature = "dns01")]
/// ACME DNS-01 support for automatic TLS.
pub mod dns01;
/// Common runtime error type.
pub mod error;
pub mod http;
/// Tracing subscriber setup helpers.
pub mod logging;
#[cfg(any(feature = "nats", feature = "sqs"))]
/// Message queue integrations.
pub mod mq;
/// Low-level networking APIs.
pub mod net;
pub(crate) mod prng;
/// Resource lifecycle integration.
pub mod resource;
pub(crate) mod resource_lifecycle;
/// Runtime configuration and shutdown control.
pub mod runtime;
pub(crate) mod runtime_state;
/// Interval and cron-style scheduling.
pub mod schedule;
/// Secret loading helpers.
pub mod secret;
mod select;
/// OS signal helpers.
pub mod signals;
/// Structured task spawning and coordination.
pub mod task;
/// Timeout helpers.
pub mod time;
/// TLS certificate and connection helpers.
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
