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
//! - [`self::Rejection`], [`self::RejectionContext`], and [`self::RequestId`]:
//!   what a router's `rejection_mapper` is given when Camber refuses a request
//! - [`self::DisconnectSignal`] and [`self::DisconnectCause`]: observe one
//!   response's lifetime through
//!   [`Request::on_disconnect`](self::Request::on_disconnect) — it resolves
//!   once, to the peer going away, this stream being reset, the server
//!   shutting down, or the response finishing
//!
//! # Two Query Views
//!
//! [`Request::query`](self::Request::query) and
//! [`Request::query_all`](self::Request::query_all) look values up by decoded
//! key. [`Request::query_pairs`](self::Request::query_pairs) iterates every
//! decoded pair in the order the peer sent it, duplicates and blank keys
//! included. [`Request::raw_query`](self::Request::raw_query) returns the query
//! exactly as it arrived, without the leading `?`.
//!
//! A request target with no `?` has no raw query. One that ends in `?` has an
//! empty one. Both yield zero decoded pairs, so `raw_query` is what separates
//! them. An empty lookup name stays absent from `query` and `query_all` even
//! though `query_pairs` exposes blank keys.
//!
//! Decoding is permissive and never fails. A valid `%HH` escape decodes one
//! byte, `+` and `%20` both decode to a space, a malformed escape stays
//! literal, and invalid UTF-8 becomes the replacement character. No pair is
//! rejected or dropped. Read `raw_query` and apply your own policy when a
//! signature check or a strict decoder needs the accepted bytes.
//!
//! The URI owns the raw query, so `raw_query` borrows it and allocates nothing.
//! The first decoded accessor parses the whole query once into a cache the
//! request owns; every later call borrows from that one sequence.
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
//!         .request_timeout(Duration::from_secs(10))
//!         .response_idle_timeout(Duration::from_secs(2))
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
//! `&Request` and a [`self::Next`] handle. [`self::MiddlewareFn`] is the shape
//! a stored frame has, [`self::MiddlewareFuture`] what one frame hands back,
//! and [`self::HandlerOutcome`] what that future resolves to.
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
pub(crate) mod body_admission;
mod boundary;
mod buffer_config;
mod checked_collect;
mod client;
/// Response compression helpers.
pub mod compression;
mod conn;
mod cookie;
/// CORS middleware builders and helpers.
pub mod cors;
mod disconnect;
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
mod multipart_route;
mod operation;
#[cfg(feature = "otel")]
/// OpenTelemetry request propagation and tracing hooks.
pub mod otel;
mod policy_value;
pub(crate) mod proxy_policy;
/// Rate limiting middleware.
pub mod rate_limit;
mod record;
mod rejection;
mod request;
mod request_budget;
mod response;
mod route_budgets;
mod router;
mod server;
mod server_lifecycle;
mod server_policy;
mod sse;
pub(crate) mod static_files;
mod stream;
mod streaming;
mod transfer_budget;
mod trie;
mod util;
/// Request validation middleware.
pub mod validate;
#[cfg(feature = "ws")]
mod websocket;
#[cfg(feature = "ws")]
mod ws_proxy;

pub use async_proxy::proxy_forward;
pub use body_admission::{BodyAdmission, BodyAdmissionContext, RequestBodyMode};
pub use boundary::{ByteBoundary, DeadlineBoundary};
/// The reference-counted byte buffer Camber's streaming request bodies hand out.
///
/// Re-exported because [`MultipartField::next_chunk`](self::MultipartField::next_chunk)
/// returns one: a public signature naming a type a caller cannot name is not a
/// usable API.
pub use bytes::Bytes;
pub use client::{
    ClientBuilder, client, delete, delete_with_body, get, head, options, patch, patch_form,
    patch_json, post, post_form, post_json, put, put_form, put_json,
};
pub use cookie::{CookieOptions, SameSite};
pub use disconnect::{DisconnectCause, DisconnectSignal};
pub use health::{ProxyHealthResource, spawn_health_checker};
pub use host_router::HostRouter;
pub use method::{Method, ParseMethodError};
pub use middleware::{MiddlewareFn, MiddlewareFuture, Next, ResponseFuture};
pub use multipart::{
    MultipartField, MultipartLimits, MultipartLimitsBuilder, MultipartReader, MultipartStream, Part,
};
pub use proxy_policy::ProxyPolicy;
pub use rejection::{
    NegotiatedResponseMetadata, Rejection, RejectionContext, RejectionKind, RejectionProtocol,
    RequestId,
};
pub use request::{Request, RequestBuilder};
pub use request_budget::RequestBudget;
pub use response::{HandlerOutcome, HeaderPair, IntoResponse, Response};
#[cfg(feature = "grpc")]
pub use router::GrpcRouter;
pub use router::Router;
pub use server::{
    ServerBuilder, ServerHandle, ServerHandleFuture, serve, serve_async, serve_async_hosts,
    serve_async_hosts_tls, serve_async_tls, serve_background, serve_background_hosts,
    serve_background_hosts_tls, serve_background_tls, serve_hosts, serve_listener, server,
    server_hosts,
};
pub use server_policy::ServerPolicy;
pub use sse::SseWriter;
pub use static_files::{serve_file, serve_file_unbounded, serve_file_with_limit};
pub use stream::{StreamResponse, StreamSender};
pub use transfer_budget::TransferBudget;
#[cfg(feature = "ws")]
pub use websocket::{WsCloseCause, WsConn, WsMessage, WsReceive, WsReceiver, WsSender};

pub(crate) use buffer_config::{BufferConfig, DEFAULT_CHANNEL_BUFFER};
pub(crate) use util::{map_reqwest_error, strip_quotes};
