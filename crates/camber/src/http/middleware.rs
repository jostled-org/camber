use super::async_proxy::ProxyRequest;
use super::rejection::{HANDLER, MIDDLEWARE, Rejected, RejectionScope};
use super::response::HandlerOutcome;
use super::trie::Handler;
use super::{Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// The future one middleware frame hands back.
///
/// It resolves to the same fallible shape a buffered handler produces: a
/// frame's failure keeps its category and source chain until the one boundary
/// that decides what the peer is told, instead of becoming a response nothing
/// can classify.
///
/// Named here rather than spelled out at each frame that builds one: a frame
/// restating this expansion keeps compiling after the outcome changes shape,
/// and only stops agreeing with the chain that awaits it.
pub type MiddlewareFuture = Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>;

/// The future one stage hands back once its answer is already settled.
///
/// Infallible, unlike [`MiddlewareFuture`]: a refusal is mapped where it was
/// raised, so what unwinds out of a chain, a terminal, or a gate is a response
/// and nothing else. Named for the reason [`MiddlewareFuture`] is — a site that
/// respells the expansion keeps compiling after the shape changes, and only
/// stops agreeing with whatever awaits it.
pub type ResponseFuture = Pin<Box<dyn Future<Output = Response> + Send>>;

/// An async middleware function that wraps request handling.
///
/// Receives the request and a `Next` handle. Returns a future that resolves to
/// a response, or to the failure this frame raised. Can short-circuit by
/// returning early without calling `next.call()`.
pub type MiddlewareFn = Box<dyn Fn(&Request, Next) -> MiddlewareFuture + Send + Sync>;

/// Terminal handler for the middleware chain.
pub(super) enum Terminal<'a> {
    /// A buffered handler.
    Handler(&'a Handler),
    /// A refusal the routing stage already decided.
    ///
    /// No handler runs, and the refusal is still mapped HERE rather than before
    /// the chain: that is what makes the mapped response unwind through the
    /// frames a selected child router entered around its own terminal.
    Rejected(Rejected),
    /// Passthrough terminal for middleware gating.
    ///
    /// Answers with a value marked as its own, which is what lets the caller
    /// tell a chain that passed from one that replaced the answer — including a
    /// frame that refused only after the terminal had already been reached.
    Gate,
    /// Proxy terminal — forwards the request to a backend without boxing a closure.
    Proxy { backend: Arc<str>, prefix: Arc<str> },
}

impl Terminal<'_> {
    /// Produce the response this terminal answers with.
    ///
    /// The policy arrives from the chain rather than being stored here, so one
    /// request carries one copy of it however deep the chain around this
    /// terminal is.
    fn run(self, req: &Request, scope: RejectionScope) -> ResponseFuture {
        match self {
            Self::Handler(handler) => {
                let outcome = handler(req);
                Box::pin(async move { scope.resolve(outcome.await, HANDLER) })
            }
            Self::Rejected(rejected) => Box::pin(async move { scope.map(rejected) }),
            Self::Gate => Box::pin(async { Response::empty_raw(200).mark_gate() }),
            Self::Proxy { backend, prefix } => Box::pin(forward_proxy(
                ProxyRequest::from_request(req),
                backend,
                prefix,
                scope,
            )),
        }
    }
}

/// Handle to the next layer in the middleware chain.
///
/// Calling `next.call(req)` returns a future that resolves to the
/// response from the remaining middleware and terminal handler.
pub struct Next<'a> {
    remaining: &'a [MiddlewareFn],
    terminal: Terminal<'a>,
    /// The policy a frame's own failure is mapped under.
    ///
    /// Carried down the chain rather than reached for at the end of it: a frame
    /// that fails is mapped where it failed, so what the frames outside it
    /// unwind through is already the mapped response.
    scope: RejectionScope,
}

impl<'a> Next<'a> {
    pub(super) fn new(
        remaining: &'a [MiddlewareFn],
        terminal: Terminal<'a>,
        scope: RejectionScope,
    ) -> Self {
        Self {
            remaining,
            terminal,
            scope,
        }
    }

    /// Run the next middleware layer or terminal handler.
    pub fn call(self, req: &Request) -> ResponseFuture {
        let Self {
            remaining,
            terminal,
            scope,
        } = self;
        match remaining.split_first() {
            Some((frame, rest)) => {
                let next = Self {
                    remaining: rest,
                    terminal,
                    scope: scope.clone(),
                };
                let entered = frame(req, next);
                Box::pin(async move { scope.resolve(entered.await, MIDDLEWARE) })
            }
            None => terminal.run(req, scope),
        }
    }
}

/// Forward a proxy request to the backend, or refuse it where it failed.
///
/// The refusal is built here rather than downstream: this is the last point
/// that knows the failure was an upstream's and not the application's, and a
/// terminal that mapped it anywhere else would unwind through frames it had
/// already left.
async fn forward_proxy(
    proxy_req: ProxyRequest,
    backend: Arc<str>,
    prefix: Arc<str>,
    scope: RejectionScope,
) -> Response {
    match super::async_proxy::forward_request_buffered(proxy_req, &backend, &prefix).await {
        Ok(resp) => resp,
        Err(failure) => scope.map(Rejected::from_proxy_failure(failure)),
    }
}
