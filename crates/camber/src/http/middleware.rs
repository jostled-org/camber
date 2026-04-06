use super::async_proxy::ProxyRequest;
use super::trie::Handler;
use super::{Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// An async middleware function that wraps request handling.
///
/// Receives the request and a `Next` handle. Returns a future
/// that resolves to a response. Can short-circuit or modify the
/// response by returning early without calling `next.call()`.
pub type MiddlewareFn =
    Box<dyn Fn(&Request, Next) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;

/// Terminal handler for the middleware chain.
pub(crate) enum Terminal<'a> {
    Handler(&'a Handler),
    /// Passthrough terminal for middleware gating.
    /// Sets the flag to `true` when reached, indicating middleware did not short-circuit.
    Gate(Arc<AtomicBool>),
    /// Proxy terminal — forwards the request to a backend without boxing a closure.
    Proxy {
        backend: Arc<str>,
        prefix: Arc<str>,
    },
}

/// Handle to the next layer in the middleware chain.
///
/// Calling `next.call(req)` returns a future that resolves to the
/// response from the remaining middleware and terminal handler.
pub struct Next<'a> {
    remaining: &'a [MiddlewareFn],
    terminal: Terminal<'a>,
}

impl<'a> Next<'a> {
    pub(crate) fn new(remaining: &'a [MiddlewareFn], terminal: Terminal<'a>) -> Self {
        Self {
            remaining,
            terminal,
        }
    }

    pub fn call(self, req: &Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        match (self.remaining.split_first(), &self.terminal) {
            (Some((mw, rest)), _) => {
                let next = Next {
                    remaining: rest,
                    terminal: self.terminal,
                };
                mw(req, next)
            }
            (None, Terminal::Handler(handler)) => handler(req),
            (None, Terminal::Gate(flag)) => {
                flag.store(true, Ordering::Release);
                Box::pin(async { Response::empty_raw(200) })
            }
            (None, Terminal::Proxy { backend, prefix }) => {
                let proxy_req = ProxyRequest::from_request(req);
                let backend = Arc::clone(backend);
                let prefix = Arc::clone(prefix);
                Box::pin(forward_proxy(proxy_req, backend, prefix))
            }
        }
    }
}

/// Forward a proxy request to the backend, mapping errors to 502 responses.
async fn forward_proxy(proxy_req: ProxyRequest, backend: Arc<str>, prefix: Arc<str>) -> Response {
    match super::async_proxy::forward_request_buffered(proxy_req, &backend, &prefix).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(error = %e, "proxy upstream failed");
            Response::text_raw(502, "proxy upstream failed")
        }
    }
}
