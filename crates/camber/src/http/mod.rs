//! HTTP server and client surface for Camber.
//!
//! This module is the main entrypoint for building services: routing,
//! middleware, request and response types, server startup, proxying, and a
//! small built-in HTTP client all live here.
//!
//! # Start With a Router
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
//! Use [`self::serve`] for the normal blocking server case. Use the `serve_async*`
//! or `serve_background*` variants when you need explicit handle management
//! inside an existing runtime scope.
//!
//! # Core Types
//!
//! - [`self::Router`]: register routes, middleware, SSE, streams, proxy routes, and
//!   feature-gated WebSocket or gRPC handlers
//! - [`self::Request`]: inspect params, query strings, headers, cookies, form data,
//!   multipart bodies, and raw bytes
//! - [`self::Response`]: build text, JSON, bytes, headers, and cookies
//! - [`self::IntoResponse`]: handler return conversion for `Response` and
//!   `Result<Response, RuntimeError>`
//!
//! # HTTP Client
//!
//! For one-off calls, use the free functions like [`self::get`], [`self::post`],
//! [`self::put`], and [`self::delete`]. For custom timeouts or retries, start
//! with [`self::client`]:
//!
//! ```rust,no_run
//! use camber::RuntimeError;
//! use camber::http;
//! use std::time::Duration;
//!
//! async fn fetch() -> Result<(), RuntimeError> {
//!     let client = http::client()
//!         .connect_timeout(Duration::from_secs(5))
//!         .read_timeout(Duration::from_secs(10))
//!         .retries(3)
//!         .backoff(Duration::from_millis(100));
//!
//!     let response = client.get("https://example.com/health").await?;
//!     let _status = response.status();
//!     Ok(())
//! }
//! ```
//!
//! # Middleware and Handler Shape
//!
//! Handlers receive `&Request` and return an async block. Middleware receives
//! `&Request` and a [`self::Next`] handle.
//!
//! If you need request data after an `.await`, move owned values into the
//! future first instead of borrowing from `req` across the await boundary.
//!
//! # Related Modules
//!
//! - [`self::cors`]: CORS helpers
//! - [`self::compression`]: response compression helpers
//! - [`self::rate_limit`]: request rate limiting middleware
//! - [`self::validate`]: request validation middleware
//! - [`self::mock`]: HTTP client interception for tests
//! - `otel`: OpenTelemetry propagation and spans when the feature is enabled

mod async_proxy;
mod body;
mod buffer_config;
mod client;
/// Response compression helpers.
pub mod compression;
mod conn;
mod cookie;
/// CORS middleware builders and helpers.
pub mod cors;
mod dispatch;
mod encoding;
#[cfg(feature = "grpc")]
mod grpc_support;
mod handle;
mod health;
mod host_router;
mod internal_routes;
mod method;
mod middleware;
/// HTTP client mocking for tests.
pub mod mock;
mod multipart;
#[cfg(feature = "otel")]
/// OpenTelemetry request propagation and tracing hooks.
pub mod otel;
/// Rate limiting middleware.
pub mod rate_limit;
mod request;
mod response;
mod router;
mod server;
mod sse;
mod static_files;
mod stream;
mod trie;
mod util;
/// Request validation middleware.
pub mod validate;
#[cfg(feature = "ws")]
mod websocket;
#[cfg(feature = "ws")]
mod ws_proxy;

pub use async_proxy::proxy_forward;
pub use client::{
    ClientBuilder, client, delete, delete_with_body, get, head, options, patch, patch_form,
    patch_json, post, post_form, post_json, put, put_form, put_json,
};
pub use cookie::{CookieOptions, SameSite};
pub use health::{ProxyHealthResource, spawn_health_checker};
pub use host_router::HostRouter;
pub use method::{Method, ParseMethodError};
pub use middleware::Next;
pub use multipart::{MultipartReader, Part};
pub use request::{Request, RequestBuilder};
pub use response::{HeaderPair, IntoResponse, Response};
#[cfg(feature = "grpc")]
pub use router::GrpcRouter;
pub use router::Router;
pub use server::{
    ServerHandle, serve, serve_async, serve_async_hosts, serve_async_hosts_tls, serve_async_tls,
    serve_background, serve_background_hosts, serve_background_hosts_tls, serve_background_tls,
    serve_hosts, serve_listener,
};
pub use sse::SseWriter;
pub use static_files::serve_file;
pub use stream::{StreamResponse, StreamSender};
#[cfg(feature = "ws")]
pub use websocket::{WsConn, WsMessage};

pub(crate) use buffer_config::{BufferConfig, DEFAULT_CHANNEL_BUFFER};
pub(crate) use util::{map_reqwest_error, strip_quotes};
