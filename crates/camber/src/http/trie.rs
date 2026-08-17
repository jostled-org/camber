use super::Request;
use super::method::Method;
use super::multipart::{MultipartLimits, MultipartStream};
use super::request::Params;
use super::response::HandlerOutcome;
use super::sse::SseWriter;
use super::stream::StreamResponse;
#[cfg(feature = "ws")]
use super::websocket::WsConn;
use crate::RuntimeError;
use arrayvec::ArrayVec;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// ── Handler type aliases ───────────────────────────────────────────

pub(super) type Handler =
    Box<dyn Fn(&Request) -> Pin<Box<dyn Future<Output = HandlerOutcome> + Send>> + Send + Sync>;
pub(super) type SseHandler = Arc<SseRegistration>;
/// One registered SSE producer, erased the way every stored handler is.
pub(super) type SseProducer =
    Box<dyn Fn(&Request, &mut SseWriter) -> Result<(), RuntimeError> + Send + Sync>;
pub(super) type StreamHandler =
    Box<dyn Fn(&Request) -> Pin<Box<dyn Future<Output = StreamResponse> + Send>> + Send + Sync>;
#[cfg(feature = "ws")]
pub(super) type WsHandler = Arc<dyn Fn(&Request, WsConn) -> Result<(), RuntimeError> + Send + Sync>;
/// One streaming multipart handler, erased the way every stored handler is.
///
/// Boxed inside its registration rather than shared on its own. What the route
/// coordinator takes a handle to is the whole registration, so the handler
/// outlives the borrow of the router that selected it without a second
/// reference count of its own.
pub(super) type MultipartHandler = Box<
    dyn Fn(&Request, MultipartStream) -> Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
        + Send
        + Sync,
>;

/// One SSE route: the producer, and the download policy its body runs under.
///
/// The policy travels with the producer because it is the registration's, not the
/// router's: two SSE routes on one router publish under their own bounds, and the
/// route class has to hand the selected pair over together. Shared behind one
/// `Arc`, so selecting this route stays a reference-count bump and
/// [`RouteHandler`] keeps the width every method slot of every frozen trie node
/// pays for.
pub(super) struct SseRegistration {
    producer: SseProducer,
    budget: super::TransferBudget,
}

impl SseRegistration {
    /// Register one producer under the policy the application chose for it.
    pub(super) fn new(producer: SseProducer, budget: super::TransferBudget) -> Self {
        Self { producer, budget }
    }

    /// The download policy this registration's body is bounded by.
    pub(super) const fn budget(&self) -> super::TransferBudget {
        self.budget
    }

    /// Run this registration's producer against the one writer it is given.
    pub(super) fn produce(
        &self,
        request: &Request,
        writer: &mut SseWriter,
    ) -> Result<(), RuntimeError> {
        (self.producer)(request, writer)
    }
}

/// One streaming multipart route: the handler, and the bounds it reads its
/// payload under.
///
/// The limits travel with the handler because they are the registration's, not
/// the router's: two multipart routes on one router read under their own
/// bounds, and classification has to hand the selected pair over together.
///
/// Shared rather than stored inline, because [`MultipartLimits`] is seven
/// `usize`s. Inline it set the width of [`RouteHandler`], and through it of
/// every method slot of every node in every frozen trie — routers with no
/// multipart route included — and classification copied all of it into the
/// route class of each matched request. Behind one `Arc`, selecting this route
/// is a reference count bump.
pub(super) struct MultipartRegistration {
    handler: MultipartHandler,
    limits: MultipartLimits,
}

impl MultipartRegistration {
    /// Register one handler under the bounds the application built for it.
    pub(super) fn new(handler: MultipartHandler, limits: MultipartLimits) -> Self {
        Self { handler, limits }
    }

    /// The bounds this registration reads its payload under.
    pub(super) fn limits(&self) -> MultipartLimits {
        self.limits
    }

    /// Enter this registration's handler with the one access handle it is given.
    pub(super) fn enter(
        &self,
        request: &Request,
        stream: MultipartStream,
    ) -> Pin<Box<dyn Future<Output = HandlerOutcome> + Send>> {
        (self.handler)(request, stream)
    }
}

/// Distinguishes normal request handlers from streaming handlers.
pub(super) enum RouteHandler {
    Async(Handler),
    Stream(StreamHandler),
    Sse(SseHandler),
    /// A streaming multipart route, held as one shared registration.
    Multipart(Arc<MultipartRegistration>),
    #[cfg(feature = "ws")]
    WebSocket(WsHandler),
    Proxy {
        backend: Arc<str>,
        prefix: Arc<str>,
        healthy: Option<Arc<AtomicBool>>,
        /// The buffered upstream maximum this route froze at registration.
        ///
        /// Frozen here rather than read from a shared policy at forward time:
        /// two routes to the same backend may name different ceilings, and a
        /// route that resolved one per request could be answered under a bound
        /// its registration never chose. `None` is the named opt-out.
        buffered_limit: Option<usize>,
    },
    ProxyStream {
        backend: Arc<str>,
        prefix: Arc<str>,
        healthy: Option<Arc<AtomicBool>>,
    },
}

impl RouteHandler {
    /// Whether a GET registration of this kind may answer a HEAD request.
    ///
    /// A streaming multipart route may not. Its handler is a session over a
    /// payload a HEAD request never sends, so the fallback ran it against an
    /// empty body: the session was abandoned, the incomplete body outranked
    /// whatever the handler said, and a plain probe was answered `400` with the
    /// connection closed under it. A node that serves GET with one therefore
    /// does not serve HEAD at all, and refuses the method naming what it does
    /// serve.
    ///
    /// Listed rather than wildcarded, so a later body-consuming class states
    /// its own answer instead of inheriting this one.
    fn answers_head(&self) -> bool {
        match self {
            Self::Async(_)
            | Self::Stream(_)
            | Self::Sse(_)
            | Self::Proxy { .. }
            | Self::ProxyStream { .. } => true,
            #[cfg(feature = "ws")]
            Self::WebSocket(_) => true,
            Self::Multipart(_) => false,
        }
    }
}

// ── Trie internals ─────────────────────────────────────────────────

/// The segments one path match captured, borrowed from the path itself.
///
/// Nothing here is owned. A method refusal collects captures and reads none of
/// them, and the selection that keeps them binds each to a name as it builds
/// [`Params`], so boxing them during the traversal spent one allocation per
/// parameter to produce values no answer reads.
type CaptureVec<'a> = Vec<&'a str>;

struct RegisteredHandler {
    method: Method,
    handler: RouteHandler,
    capture_names: Box<[Arc<str>]>,
    route: Arc<str>,
}

type MethodHandlers = Vec<RegisteredHandler>;

/// Every routing method, in the order an `Allow` header lists them.
///
/// Not `Method::ordinal` order: that one is a dispatch index, and reusing it
/// here would make the header's order an accident of how the array is laid out.
///
/// Shared with route registration, which needs the same set for a different
/// reason: a proxy prefix claims every method Camber routes on. A second
/// literal there would go on claiming seven methods after an eighth variant was
/// added, and nothing at runtime could see the omission — the added method
/// would simply stop being proxied. This one is length-checked against
/// [`Method::COUNT`] and proved a permutation below.
pub(super) const CANONICAL_METHODS: [Method; Method::COUNT] = [
    Method::Get,
    Method::Head,
    Method::Post,
    Method::Put,
    Method::Patch,
    Method::Delete,
    Method::Options,
];

/// The methods one handler set serves, one bit per `Method::ordinal()`.
///
/// Two branches can claim one concrete path, and the refusal they share has to
/// name what both serve. Merging rendered `Allow` values would mean parsing
/// them back; merging bits is one `or`.
type MethodMask = u8;

/// `MethodMask` holds every method.
const _: () = assert!(
    Method::COUNT <= MethodMask::BITS as usize,
    "MethodMask has fewer bits than there are methods"
);

/// `CANONICAL_METHODS` names every routing method exactly once.
///
/// The array's length is already the variant count, so an entry written twice
/// silently drops the method it displaced from every `Allow` header this
/// module renders. Nothing at runtime can see that omission — the header is
/// simply short — so the permutation is proved here instead.
const _: () = {
    let mut listed = [false; Method::COUNT];
    let mut position = 0;
    while position < Method::COUNT {
        let ordinal = CANONICAL_METHODS[position].ordinal();
        assert!(!listed[ordinal], "CANONICAL_METHODS lists a method twice");
        listed[ordinal] = true;
        position += 1;
    }
};

/// What one path-and-method lookup established.
pub(super) enum RouteLookup<'n, 'p> {
    /// A handler claims this path with this method.
    Matched(Selected<'n, 'p>),
    /// A route claims this path, but not with this method.
    MethodMismatch {
        route: Arc<str>,
        /// Everything the branches claiming this path serve, between them.
        ///
        /// Shared rather than borrowed from one node: a path only one branch
        /// claims is answered with the value that node rendered at freeze, so
        /// the refusal carrying it costs a refcount bump. A path two branches
        /// claim has no single frozen value to name, so that set is rendered
        /// into the same shape — a refusal must not have to know which of the
        /// two it was handed.
        allow: Arc<str>,
    },
    /// No route claims this path.
    Unmatched,
}

/// What a method-selection pass found, and nothing a refusal would need.
///
/// Named apart from [`RouteLookup`] so the callers that only dispatch — they
/// have no refusal to explain — can ask for selection alone and skip the
/// collecting pass and the `Allow` merge that explaining one costs.
pub(super) struct Selected<'n, 'p> {
    /// The normalized pattern the selected handler was registered under.
    ///
    /// Borrowed from the frozen handler, like everything else here. Both
    /// callers clone it into an identity that outlives this value, so owning it
    /// spent a refcount bump per selection to hand back a handle each of them
    /// then took its own copy of anyway.
    pub(super) route: &'n Arc<str>,
    pub(super) handler: &'n RouteHandler,
    /// The names the matched pattern spells its captures with.
    capture_names: &'n [Arc<str>],
    /// The segments this path matched, in the pattern's own order.
    captures: CaptureVec<'p>,
}

impl Selected<'_, '_> {
    /// Bind each captured segment to the name its pattern spells it with.
    ///
    /// Held back for the one caller that keeps the parameters, because most do
    /// not: pre-body classification reads the handler and the route and
    /// discards the rest, and binding there boxed a string per parameter for
    /// every buffered, head-only, and refused request to throw all of them
    /// away.
    pub(super) fn bind_params(self) -> Params {
        debug_assert_eq!(
            self.capture_names.len(),
            self.captures.len(),
            "a pattern names one capture per segment its match collected"
        );
        self.capture_names
            .iter()
            .cloned()
            .zip(self.captures)
            .map(|(name, value)| (name, Box::from(value)))
            .collect()
    }
}

/// A segment in a route pattern: literal, named parameter, or wildcard catch-all.
enum Segment {
    Static(Box<str>),
    Param(Box<str>),
    Wildcard(Box<str>),
}

impl Segment {
    /// Render this segment the way the pattern that produced it spelled it.
    fn render(&self, into: &mut String) {
        match self {
            Self::Static(name) => into.push_str(name),
            Self::Param(name) => {
                into.push(':');
                into.push_str(name);
            }
            Self::Wildcard(name) => {
                into.push('*');
                into.push_str(name);
            }
        }
    }
}

fn parse_segments(path: &str) -> Box<[Segment]> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| match (s.strip_prefix('*'), s.strip_prefix(':')) {
            (Some(name), _) => Segment::Wildcard(name.into()),
            (_, Some(name)) => Segment::Param(name.into()),
            _ => Segment::Static(s.into()),
        })
        .collect()
}

/// The registered pattern, spelled the way the trie actually matches it.
///
/// Rebuilt from the parsed segments rather than kept as written, so `//a//b/`
/// and `/a/b` — which match identically — also name themselves identically.
/// This is the value a rejection context reports, so two spellings of one route
/// must not read as two routes.
fn normalize_route(segments: &[Segment]) -> Arc<str> {
    match segments.is_empty() {
        true => Arc::from("/"),
        false => {
            let mut pattern = String::new();
            for segment in segments {
                pattern.push('/');
                segment.render(&mut pattern);
            }
            Arc::from(pattern.as_str())
        }
    }
}

/// Mutable trie node used during route registration.
pub(crate) struct TrieNode {
    static_children: BTreeMap<Box<str>, TrieNode>,
    param_child: Option<Box<TrieNode>>,
    wildcard: Option<MethodHandlers>,
    handlers: MethodHandlers,
}

impl TrieNode {
    pub(crate) fn new() -> Self {
        Self {
            static_children: BTreeMap::new(),
            param_child: None,
            wildcard: None,
            handlers: Vec::new(),
        }
    }

    pub(crate) fn insert_route(&mut self, method: Method, path: &str, handler: RouteHandler) {
        let segments = parse_segments(path);
        let capture_names = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Param(name) | Segment::Wildcard(name) => {
                    Some(Arc::<str>::from(name.as_ref()))
                }
                Segment::Static(_) => None,
            })
            .collect();
        let registered = RegisteredHandler {
            method,
            handler,
            capture_names,
            route: normalize_route(&segments),
        };
        self.insert_segments(&segments, registered);
    }

    fn insert_segments(&mut self, segments: &[Segment], registered: RegisteredHandler) {
        match segments.first() {
            None => self.handlers.push(registered),
            Some(Segment::Static(name)) => {
                let child = self
                    .static_children
                    .entry(name.clone())
                    .or_insert_with(TrieNode::new);
                child.insert_segments(&segments[1..], registered);
            }
            Some(Segment::Param(_)) => {
                let child = self
                    .param_child
                    .get_or_insert_with(|| Box::new(TrieNode::new()));
                child.insert_segments(&segments[1..], registered);
            }
            Some(Segment::Wildcard(_)) => {
                self.wildcard.get_or_insert_with(Vec::new).push(registered);
            }
        }
    }

    /// Freeze into an immutable trie for serving.
    pub(crate) fn freeze(self) -> FrozenNode {
        let static_children: Box<[(Box<str>, FrozenNode)]> = self
            .static_children
            .into_iter()
            .map(|(k, v)| (k, v.freeze()))
            .collect();

        FrozenNode {
            static_children,
            param_child: self.param_child.map(|node| Box::new(node.freeze())),
            wildcard: self.wildcard.map(freeze_handlers),
            handlers: freeze_handlers(self.handlers),
        }
    }
}

/// Index one node's registered handlers by `Method::ordinal()`.
///
/// Everything a method refusal at this node reports is rendered here too. The
/// allowed set and the pattern a `405` names are fixed the moment the trie
/// freezes, so deriving them per refusal was seven probes, a collection, and a
/// join to rebuild a string that never changes.
fn freeze_handlers(registered: MethodHandlers) -> FrozenMethodHandlers {
    let mut by_method: MethodSlots = Default::default();
    for entry in registered {
        by_method[entry.method.ordinal()] = Some(FrozenHandler {
            handler: entry.handler,
            capture_names: entry.capture_names,
            route: entry.route,
        });
    }
    FrozenMethodHandlers {
        claimed: freeze_claim(&by_method),
        by_method,
    }
}

/// What this node answers a method it does not serve with, if it claims a path.
///
/// `None` is a node no handler was registered on — an interior node on the way
/// to one that was. Such a node claims nothing, so it names no route and
/// allows no method.
fn freeze_claim(by_method: &MethodSlots) -> Option<Claimed> {
    let route = CANONICAL_METHODS
        .iter()
        .find_map(|method| by_method[method.ordinal()].as_ref())
        .map(|handler| Arc::clone(&handler.route))?;
    let served = served_mask(by_method);
    Some(Claimed {
        route,
        allow: render_allow(served),
        served,
    })
}

/// The set of methods a node answers.
///
/// `HEAD` is in it exactly when selection would answer one — for a registered
/// `HEAD`, and for a `GET` registration [`head_from_get`] admits — because a
/// method the `Allow` value names is a method the peer may then send.
fn served_mask(by_method: &MethodSlots) -> MethodMask {
    CANONICAL_METHODS
        .into_iter()
        .filter(|method| slot_serves(by_method, *method))
        .fold(0, |mask, method| mask | method_bit(method))
}

/// The bit one method occupies in a [`MethodMask`].
fn method_bit(method: Method) -> MethodMask {
    1 << method.ordinal()
}

/// Render an allowed set as an `Allow` value, in canonical order.
///
/// Folded straight into the value, the shape [`normalize_route`] uses.
/// Collecting the names first and joining them allocated a slice nothing else
/// read, and `merge_allow` reaches here per request whenever two branches
/// claiming one path serve different method sets.
fn render_allow(served: MethodMask) -> Arc<str> {
    let rendered = CANONICAL_METHODS
        .into_iter()
        .filter(|method| served & method_bit(*method) != 0)
        .fold(String::new(), |mut rendered, method| {
            let separator = match rendered.is_empty() {
                true => "",
                false => ", ",
            };
            rendered.push_str(separator);
            rendered.push_str(method.as_str());
            rendered
        });
    Arc::from(rendered.as_str())
}

/// One registered handler, with everything selecting it needs.
struct FrozenHandler {
    handler: RouteHandler,
    capture_names: Box<[Arc<str>]>,
    /// The normalized pattern this handler was registered under.
    ///
    /// `Arc<str>` because one immutable registered pattern is cloned across
    /// every concurrent request that matches it, and because two handlers on
    /// one path may spell their captures differently — the pattern belongs to
    /// the handler, not to the node.
    route: Arc<str>,
}

/// One slot per HTTP method, indexed by `Method::ordinal()`.
type MethodSlots = [Option<FrozenHandler>; Method::COUNT];

/// What a node that claims its path answers an unserved method with.
///
/// Both values are decided when the trie freezes: the pattern a refusal names,
/// and the `Allow` value it must carry. Held together because a `405` reports
/// both or neither.
struct Claimed {
    /// The pattern a refusal reports.
    ///
    /// Two handlers on one path may spell their captures differently, so a
    /// refusal has to choose. It reports the first in canonical order — the
    /// same order the `Allow` header lists — rather than the received path,
    /// which is not a registered pattern at all.
    route: Arc<str>,
    /// The frozen allowed set, already rendered as an `Allow` value.
    ///
    /// Shared, so the path only this node claims is refused with the value
    /// rendered here rather than one built per refusal.
    allow: Arc<str>,
    /// The same set as bits, for the refusal that has to merge two nodes'.
    served: MethodMask,
}

/// One node's handlers, and what it answers a method it does not serve with.
struct FrozenMethodHandlers {
    by_method: MethodSlots,
    /// Present exactly when some handler claims this node's path.
    claimed: Option<Claimed>,
}

/// Immutable trie node for request dispatch.
pub(crate) struct FrozenNode {
    static_children: Box<[(Box<str>, FrozenNode)]>,
    param_child: Option<Box<FrozenNode>>,
    wildcard: Option<FrozenMethodHandlers>,
    handlers: FrozenMethodHandlers,
}

/// What a traversal does with each candidate node's handler set.
///
/// One traversal serves both passes because they differ only here: selection
/// asks for a node that serves the received method and stops at the first
/// `true`, while a refusal — reached only once selection found nothing —
/// records what each candidate claims and answers `false`, so the walk carries
/// on and every branch that could serve the path is seen. Two traversals would
/// be the same precedence rules written twice, and a route's fallback order is
/// exactly what would drift between the copies.
type Accepts<'v, 'n> = &'v mut dyn FnMut(&'n FrozenMethodHandlers) -> bool;

/// What one path resolution found: the node's handlers and its raw captures.
type PathMatch<'n, 'p> = (&'n FrozenMethodHandlers, CaptureVec<'p>);

impl FrozenNode {
    /// Match a path, then select a handler for the received method.
    ///
    /// Priority is static children, then the param child, then the wildcard
    /// catch-all — for the method pass and the any-method pass alike, so a
    /// refusal names the same route a match would have named.
    ///
    /// `method` is `None` for a method outside Camber's route enum. Such a
    /// request selects no handler, but a route that claims its path still names
    /// itself and its allowed set.
    pub(super) fn lookup<'n, 'p>(
        &'n self,
        method: Option<Method>,
        path: &'p str,
        segments: &[&'p str],
    ) -> RouteLookup<'n, 'p> {
        match method.and_then(|method| self.select(method, path, segments)) {
            Some(selected) => RouteLookup::Matched(selected),
            None => self.refuse_method(path, segments),
        }
    }

    /// Select the handler that serves this method, with its captures bound.
    ///
    /// Public to the module because dispatch asks only this: a request being
    /// routed has no refusal to explain, and paying for the any-method pass and
    /// the `Allow` rendering to throw both away is what the split avoids.
    pub(super) fn select<'n, 'p>(
        &'n self,
        method: Method,
        path: &'p str,
        segments: &[&'p str],
    ) -> Option<Selected<'n, 'p>> {
        let mut accepts =
            |handlers: &'n FrozenMethodHandlers| slot_serves(&handlers.by_method, method);
        let (handlers, mut captures) = self.resolve(&mut accepts, path, segments)?;
        let selected = select_slot(&handlers.by_method, method)?;
        captures.reverse();
        Some(Selected {
            route: &selected.route,
            handler: &selected.handler,
            capture_names: &selected.capture_names,
            captures,
        })
    }

    /// Name the route that claims this path, and everything it may be asked for.
    ///
    /// Collects instead of stopping at the first claim. A static child and a
    /// param child can both claim one concrete path, and each serves its own
    /// methods there — a request for the other branch's method succeeds. So a
    /// refusal that named only the branch it reached first would answer with an
    /// allowed set the peer can disprove with its next request. The route is
    /// the first in precedence order, the one a match would have named.
    fn refuse_method<'n, 'p>(&'n self, path: &'p str, segments: &[&'p str]) -> RouteLookup<'n, 'p> {
        let mut claims: Vec<&'n Claimed> = Vec::new();
        let mut collect = |handlers: &'n FrozenMethodHandlers| {
            claims.extend(handlers.claimed.as_ref());
            false
        };
        let matched = self.resolve(&mut collect, path, segments);
        debug_assert!(matched.is_none(), "a collecting pass accepts no node");

        match claims.split_first() {
            Some((first, rest)) => RouteLookup::MethodMismatch {
                route: Arc::clone(&first.route),
                allow: merge_allow(first, rest),
            },
            None => RouteLookup::Unmatched,
        }
    }

    fn resolve<'n, 'p>(
        &'n self,
        accepts: Accepts<'_, 'n>,
        path: &'p str,
        segments: &[&'p str],
    ) -> Option<PathMatch<'n, 'p>> {
        let Some(&segment) = segments.first() else {
            return accepts(&self.handlers).then(|| (&self.handlers, CaptureVec::new()));
        };
        let rest = &segments[1..];
        if let Some(matched) = self.resolve_static(accepts, path, segment, rest) {
            return Some(matched);
        }
        if let Some(matched) = self.resolve_param(accepts, path, segment, rest) {
            return Some(matched);
        }
        self.resolve_wildcard(accepts, path, segments)
    }

    fn resolve_static<'n, 'p>(
        &'n self,
        accepts: Accepts<'_, 'n>,
        path: &'p str,
        segment: &str,
        rest: &[&'p str],
    ) -> Option<PathMatch<'n, 'p>> {
        let idx = self
            .static_children
            .binary_search_by_key(&segment, |(k, _)| k)
            .ok()?;
        self.static_children[idx].1.resolve(accepts, path, rest)
    }

    fn resolve_param<'n, 'p>(
        &'n self,
        accepts: Accepts<'_, 'n>,
        path: &'p str,
        segment: &'p str,
        rest: &[&'p str],
    ) -> Option<PathMatch<'n, 'p>> {
        let child = self.param_child.as_ref()?;
        let mut result = child.resolve(accepts, path, rest)?;
        result.1.push(segment);
        Some(result)
    }

    fn resolve_wildcard<'n, 'p>(
        &'n self,
        accepts: Accepts<'_, 'n>,
        path: &'p str,
        segments: &[&'p str],
    ) -> Option<PathMatch<'n, 'p>> {
        let handlers = self.wildcard.as_ref()?;
        accepts(handlers).then(|| (handlers, vec![wildcard_span(path, segments)]))
    }
}

/// The `Allow` value for every branch that claims one path.
///
/// One claim — or several that happen to serve the same set — is answered with
/// the value its node rendered at freeze, so the refusal carrying it costs a
/// refcount bump. A set no single node rendered has no frozen value to name, so
/// it is built here, on the refusal path only.
fn merge_allow(first: &Claimed, rest: &[&Claimed]) -> Arc<str> {
    let served = rest
        .iter()
        .fold(first.served, |mask, claim| mask | claim.served);
    match served == first.served {
        true => Arc::clone(&first.allow),
        false => render_allow(served),
    }
}

/// Whether a slot set answers this method at all.
///
/// Reads the slots rather than the node, so freeze can ask it while the node
/// that will hold them does not exist yet.
fn slot_serves(by_method: &MethodSlots, method: Method) -> bool {
    select_slot(by_method, method).is_some()
}

/// Read one method's slot, falling back from HEAD to GET.
fn select_slot(by_method: &MethodSlots, method: Method) -> Option<&FrozenHandler> {
    by_method[method.ordinal()]
        .as_ref()
        .or_else(|| match method {
            Method::Head => head_from_get(by_method),
            _ => None,
        })
}

/// The GET handler a HEAD request may be answered from.
///
/// Read by selection and by the `Allow` value alike, so a GET registration that
/// cannot answer a HEAD does not advertise one either.
fn head_from_get(by_method: &MethodSlots) -> Option<&FrozenHandler> {
    let registered = by_method[Method::Get.ordinal()].as_ref()?;
    registered.handler.answers_head().then_some(registered)
}

/// Extract the wildcard portion of a path by computing the span from the first
/// segment to the last. The segments are slices of `path`, so pointer arithmetic
/// recovers the substring (including separators) without allocation.
fn wildcard_span<'a>(path: &'a str, segments: &[&'a str]) -> &'a str {
    match (segments.first(), segments.last()) {
        (Some(first), Some(last)) => {
            let start = first.as_ptr() as usize - path.as_ptr() as usize;
            let end = last.as_ptr() as usize - path.as_ptr() as usize + last.len();
            &path[start..end]
        }
        _ => "",
    }
}

/// How many path segments the router will match before refusing the target.
pub(super) const PATH_SEGMENT_LIMIT: usize = 32;

/// Split a URL path into non-empty segments, capped at [`PATH_SEGMENT_LIMIT`].
/// Returns `None` if the path exceeds that many segments.
pub(super) fn split_path_segments(path: &str) -> Option<ArrayVec<&str, PATH_SEGMENT_LIMIT>> {
    let mut segments = ArrayVec::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        match segments.try_push(seg) {
            Ok(()) => {}
            Err(_) => return None,
        }
    }
    Some(segments)
}
