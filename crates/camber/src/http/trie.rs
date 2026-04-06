use super::Request;
use super::Response;
use super::method::Method;
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
    Box<dyn Fn(&Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync>;
pub(super) type SseHandler =
    Arc<dyn Fn(&Request, &mut SseWriter) -> Result<(), RuntimeError> + Send + Sync>;
pub(super) type StreamHandler =
    Box<dyn Fn(&Request) -> Pin<Box<dyn Future<Output = StreamResponse> + Send>> + Send + Sync>;
#[cfg(feature = "ws")]
pub(super) type WsHandler = Arc<dyn Fn(&Request, WsConn) -> Result<(), RuntimeError> + Send + Sync>;

/// Distinguishes normal request handlers from streaming handlers.
pub(super) enum RouteHandler {
    Async(Handler),
    Stream(StreamHandler),
    Sse(SseHandler),
    #[cfg(feature = "ws")]
    WebSocket(WsHandler),
    Proxy {
        backend: Arc<str>,
        prefix: Arc<str>,
        healthy: Option<Arc<AtomicBool>>,
    },
    ProxyStream {
        backend: Arc<str>,
        prefix: Arc<str>,
        healthy: Option<Arc<AtomicBool>>,
    },
}

// ── Trie internals ─────────────────────────────────────────────────

type Params = Box<[(Arc<str>, Box<str>)]>;
type ParamVec = Vec<(Arc<str>, Box<str>)>;
type MethodHandlers = Vec<(Method, RouteHandler)>;
type LookupResult<'a> = Option<(&'a RouteHandler, Params)>;

/// A segment in a route pattern: literal, named parameter, or wildcard catch-all.
enum Segment {
    Static(Box<str>),
    Param(Box<str>),
    Wildcard(Box<str>),
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

/// Mutable trie node used during route registration.
pub(crate) struct TrieNode {
    static_children: BTreeMap<Box<str>, TrieNode>,
    param_child: Option<(Box<str>, Box<TrieNode>)>,
    wildcard: Option<(Box<str>, MethodHandlers)>,
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
        self.insert_segments(method, &segments, handler);
    }

    fn insert_segments(&mut self, method: Method, segments: &[Segment], handler: RouteHandler) {
        match segments.first() {
            None => {
                self.handlers.push((method, handler));
            }
            Some(Segment::Static(name)) => {
                let child = self
                    .static_children
                    .entry(name.clone())
                    .or_insert_with(TrieNode::new);
                child.insert_segments(method, &segments[1..], handler);
            }
            Some(Segment::Param(name)) => {
                let (_, child) = self
                    .param_child
                    .get_or_insert_with(|| (name.clone(), Box::new(TrieNode::new())));
                child.insert_segments(method, &segments[1..], handler);
            }
            Some(Segment::Wildcard(name)) => {
                let (_, handlers) = self
                    .wildcard
                    .get_or_insert_with(|| (name.clone(), Vec::new()));
                handlers.push((method, handler));
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

        let param_child = self
            .param_child
            .map(|(name, node)| (Arc::<str>::from(&*name), Box::new(node.freeze())));

        let wildcard = self.wildcard.map(|(name, handlers)| {
            let arc_name: Arc<str> = Arc::from(&*name);
            let mut method_array: FrozenMethodHandlers = Default::default();
            for (method, handler) in handlers {
                method_array[method.ordinal()] = Some(handler);
            }
            (arc_name, method_array)
        });

        let mut handlers: FrozenMethodHandlers = Default::default();
        for (method, handler) in self.handlers {
            handlers[method.ordinal()] = Some(handler);
        }

        FrozenNode {
            static_children,
            param_child,
            wildcard,
            handlers,
        }
    }
}

/// Method handlers indexed by `Method::ordinal()`. One slot per HTTP method.
type FrozenMethodHandlers = [Option<RouteHandler>; Method::COUNT];

/// Immutable trie node for request dispatch.
pub(crate) struct FrozenNode {
    static_children: Box<[(Box<str>, FrozenNode)]>,
    param_child: Option<(Arc<str>, Box<FrozenNode>)>,
    wildcard: Option<(Arc<str>, FrozenMethodHandlers)>,
    handlers: FrozenMethodHandlers,
}

impl FrozenNode {
    /// Match a path and return the route handler + extracted params.
    /// Priority: static children > param child > wildcard catch-all.
    pub(crate) fn lookup(&self, method: Method, path: &str, segments: &[&str]) -> LookupResult<'_> {
        let (handler, params) = self.resolve(method, path, segments)?;
        Some((handler, params.into_boxed_slice()))
    }

    fn resolve(
        &self,
        method: Method,
        path: &str,
        segments: &[&str],
    ) -> Option<(&RouteHandler, ParamVec)> {
        match segments.first() {
            None => {
                let handler = self.handler_for(method)?;
                Some((handler, ParamVec::new()))
            }
            Some(segment) => self
                .resolve_static(method, path, segment, &segments[1..])
                .or_else(|| self.resolve_param(method, path, segment, &segments[1..]))
                .or_else(|| self.resolve_wildcard(method, path, segments)),
        }
    }

    /// Return the handler for a method, falling back from HEAD to GET.
    fn handler_for(&self, method: Method) -> Option<&RouteHandler> {
        lookup_method_handler(&self.handlers, method)
    }

    fn resolve_static(
        &self,
        method: Method,
        path: &str,
        segment: &str,
        rest: &[&str],
    ) -> Option<(&RouteHandler, ParamVec)> {
        let idx = self
            .static_children
            .binary_search_by_key(&segment, |(k, _)| k)
            .ok()?;
        self.static_children[idx].1.resolve(method, path, rest)
    }

    fn resolve_param(
        &self,
        method: Method,
        path: &str,
        segment: &str,
        rest: &[&str],
    ) -> Option<(&RouteHandler, ParamVec)> {
        let (param_name, child) = self.param_child.as_ref()?;
        let mut result = child.resolve(method, path, rest)?;
        result.1.push((Arc::clone(param_name), segment.into()));
        Some(result)
    }

    fn resolve_wildcard(
        &self,
        method: Method,
        path: &str,
        segments: &[&str],
    ) -> Option<(&RouteHandler, ParamVec)> {
        let (wildcard_name, handlers) = self.wildcard.as_ref()?;
        let handler = lookup_method_handler(handlers, method)?;
        let captured: Box<str> = wildcard_span(path, segments).into();
        let params = vec![(Arc::clone(wildcard_name), captured)];
        Some((handler, params))
    }
}

/// Look up a handler by method, falling back from HEAD to GET.
fn lookup_method_handler(handlers: &FrozenMethodHandlers, method: Method) -> Option<&RouteHandler> {
    handlers[method.ordinal()]
        .as_ref()
        .or_else(|| match method {
            Method::Head => handlers[Method::Get.ordinal()].as_ref(),
            _ => None,
        })
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

/// Split a URL path into non-empty segments, capped at 32.
/// Returns `None` if the path exceeds 32 segments (414 URI Too Long).
pub(super) fn split_path_segments(path: &str) -> Option<ArrayVec<&str, 32>> {
    let mut segments = ArrayVec::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        match segments.try_push(seg) {
            Ok(()) => {}
            Err(_) => return None,
        }
    }
    Some(segments)
}
