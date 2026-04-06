mod async_proxy;
mod body;
mod buffer_config;
mod client;
pub mod compression;
mod conn;
mod cookie;
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
pub mod mock;
mod multipart;
#[cfg(feature = "otel")]
pub mod otel;
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
