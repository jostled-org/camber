use super::async_proxy::ProxyRequest;
use super::rejection::{HANDLER, MIDDLEWARE, Rejected, RejectionScope};
use super::response::HandlerOutcome;
use super::trie::Handler;
use super::{Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// Answers with the provisional head a gated class shows its chain, marked
    /// as this terminal's own. The mark is what lets the caller tell a chain
    /// that passed from one that replaced the answer — including a frame that
    /// refused only after the terminal had already been reached — and what a
    /// passing chain states over the provisional head becomes the projection the
    /// real head is merged with.
    Gate,
    /// Proxy terminal — forwards the request to a backend without boxing a closure.
    ///
    /// The buffered maximum arrives from the route that froze it, so this
    /// terminal names the ceiling its own registration chose rather than a
    /// process-wide one every proxied route would share.
    Proxy {
        backend: Arc<str>,
        prefix: Arc<str>,
        /// The upstream owner this route froze: its client, its phase
        /// deadlines, and the maximum it collects an answer under.
        upstream: Arc<super::proxy_upstream::ProxyUpstream>,
    },
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
            Self::Gate => Box::pin(async {
                Response::empty_raw(super::head_projection::PROVISIONAL_STATUS).mark_gate()
            }),
            Self::Proxy {
                backend,
                prefix,
                upstream,
            } => Box::pin(forward_proxy(
                ProxyRequest::from_request(req),
                backend,
                prefix,
                upstream,
                scope,
            )),
        }
    }
}

/// Whether a chain reached the terminal that owns its answer.
///
/// A buffered route's terminal produces the real head, so a chain that answered
/// without entering it produced that head itself and is the response's origin. A
/// gate chain reads the opposite way — its terminal's value is provisional, and
/// a frame replacing it is exactly what an answer looks like — which is why
/// [`Gated`](super::head_projection::Gated) decides from the answer's own
/// provenance instead. The two questions are different, so they are asked
/// differently.
#[derive(Clone)]
pub(super) enum TerminalEntry {
    /// Nothing stands in front of the terminal, so it answers or nothing does.
    ///
    /// Both the empty chain and the answers built with no chain at all: a
    /// mapped routing refusal has no frame that could have replaced it.
    Direct,
    /// A chain stands in front of it, and marks this cell on the way in.
    Chained(Arc<AtomicBool>),
}

impl TerminalEntry {
    /// A cell no chain has reached its terminal through yet.
    fn pending() -> Self {
        Self::Chained(Arc::new(AtomicBool::new(false)))
    }

    /// Record that the chain reached its terminal.
    fn reached(&self) {
        match self {
            Self::Direct => {}
            Self::Chained(cell) => cell.store(true, Ordering::Release),
        }
    }

    /// Whether the terminal produced this chain's answer.
    pub(super) fn was_reached(&self) -> bool {
        match self {
            Self::Direct => true,
            Self::Chained(cell) => cell.load(Ordering::Acquire),
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
    /// The cell this chain marks when it reaches its terminal.
    entry: TerminalEntry,
}

impl<'a> Next<'a> {
    pub(super) fn new(
        remaining: &'a [MiddlewareFn],
        terminal: Terminal<'a>,
        scope: RejectionScope,
    ) -> Self {
        // An empty chain has nothing that could short-circuit its terminal, so
        // it costs no cell to say so.
        let entry = match remaining.is_empty() {
            true => TerminalEntry::Direct,
            false => TerminalEntry::pending(),
        };
        Self {
            remaining,
            terminal,
            scope,
            entry,
        }
    }

    /// A chain whose caller never asks which owner answered it.
    ///
    /// The gate path is the one that does not ask: what it reads is the answer's
    /// own provenance, decided by [`Gated`](super::head_projection::Gated) from
    /// the provisional head the gate terminal marks. Minting a cell for it put an
    /// allocation and an atomic store on every gated request — every WebSocket,
    /// event stream, streaming forward, and multipart session behind a router
    /// with middleware — for a fact nothing ever loads.
    pub(super) fn untracked(
        remaining: &'a [MiddlewareFn],
        terminal: Terminal<'a>,
        scope: RejectionScope,
    ) -> Self {
        Self {
            remaining,
            terminal,
            scope,
            entry: TerminalEntry::Direct,
        }
    }

    /// The signal that says whether this chain's terminal produced its answer.
    ///
    /// Read before [`Self::call`] consumes the chain, because the answer is what
    /// the caller has left afterwards and the answer cannot say who built it: a
    /// frame that replaced the terminal's response returns the same shape the
    /// terminal would have.
    pub(super) fn entry(&self) -> TerminalEntry {
        self.entry.clone()
    }

    /// Run the next middleware layer or terminal handler.
    pub fn call(self, req: &Request) -> ResponseFuture {
        let Self {
            remaining,
            terminal,
            scope,
            entry,
        } = self;
        match remaining.split_first() {
            Some((frame, rest)) => {
                let next = Self {
                    remaining: rest,
                    terminal,
                    scope: scope.clone(),
                    entry,
                };
                let entered = frame(req, next);
                Box::pin(async move { scope.resolve(entered.await, MIDDLEWARE) })
            }
            None => {
                entry.reached();
                terminal.run(req, scope)
            }
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
    upstream: Arc<super::proxy_upstream::ProxyUpstream>,
    scope: RejectionScope,
) -> Response {
    let target = super::async_proxy::ProxyTarget {
        backend: &backend,
        prefix: &prefix,
        upstream: &upstream,
    };
    match super::async_proxy::forward_request_buffered(proxy_req, &target).await {
        Ok(resp) => resp,
        Err(failure) => scope.map(Rejected::from_proxy_failure(failure)),
    }
}
