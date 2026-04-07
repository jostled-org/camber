use super::Response;
use super::request::RequestHead;
use super::router::{FrozenRouter, Router};
use super::{BufferConfig, Request};

impl std::fmt::Debug for HostRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRouter")
            .field("host_count", &self.hosts.len())
            .field("has_default", &self.default.is_some())
            .field("buffers", &self.buffers)
            .finish()
    }
}

/// Build-time host-based router. Maps hostnames to Router instances.
#[derive(Default)]
pub struct HostRouter {
    hosts: Vec<(Box<str>, Router)>,
    default: Option<Router>,
    buffers: BufferConfig,
}

impl HostRouter {
    /// Create an empty host router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the maximum request body size in bytes (capped at 256 MB).
    #[must_use]
    pub fn max_request_body(mut self, bytes: usize) -> Self {
        self.buffers = self.buffers.with_max_request_body(bytes);
        self
    }

    /// Set the channel buffer size for SSE connections.
    ///
    /// Controls how many events can be queued before backpressure applies.
    /// Default: 32.
    #[must_use]
    pub fn sse_buffer_size(mut self, size: usize) -> Self {
        self.buffers = self.buffers.with_sse_buffer_size(size);
        self
    }

    /// Set the channel buffer size for WebSocket connections.
    ///
    /// Controls how many messages can be queued in each direction before
    /// backpressure applies. Default: 32.
    #[cfg(feature = "ws")]
    #[must_use]
    pub fn ws_buffer_size(mut self, size: usize) -> Self {
        self.buffers = self.buffers.with_ws_buffer_size(size);
        self
    }

    pub(super) fn buffer_config(&self) -> BufferConfig {
        self.buffers
    }

    /// Register a router for a specific host name.
    ///
    /// Host matching is case-insensitive.
    pub fn add(&mut self, host: &str, router: Router) -> &mut Self {
        let normalized: Box<str> = host.to_ascii_lowercase().into_boxed_str();
        self.hosts.push((normalized, router));
        self
    }

    /// Set the fallback router used when no host-specific router matches.
    pub fn set_default(&mut self, router: Router) -> &mut Self {
        self.default = Some(router);
        self
    }

    pub(super) fn freeze(self) -> FrozenHostRouter {
        let mut hosts: Vec<(Box<str>, FrozenRouter)> = self
            .hosts
            .into_iter()
            .map(|(host, router)| (host, router.freeze()))
            .collect();
        hosts.sort_by(|(a, _), (b, _)| a.cmp(b));
        let default = self.default.map(Router::freeze);
        FrozenHostRouter {
            hosts: hosts.into_boxed_slice(),
            default,
        }
    }
}

/// Immutable host-based router. Dispatches by Host header.
pub(super) struct FrozenHostRouter {
    hosts: Box<[(Box<str>, FrozenRouter)]>,
    default: Option<FrozenRouter>,
}

/// Reject hosts containing path separators or control characters
/// that could be used for request smuggling.
fn is_valid_host(host: &str) -> bool {
    !host.bytes().any(|b| matches!(b, b'/' | b'\\' | 0..=31))
}

/// Extract the hostname from a Host header value, stripping the port if present.
///
/// Handles IPv6 bracketed addresses: `[::1]:8080` -> `[::1]`.
/// Handles IPv4/hostname: `example.com:8080` -> `example.com`.
fn strip_host_port(host: &str) -> &str {
    match (host.starts_with('['), host.find(']')) {
        // IPv6 bracketed: host portion ends at ']'
        (true, Some(end)) => &host[..=end],
        (true, None) => host,
        (false, _) => host.rsplit_once(':').map_or(host, |(h, _)| h),
    }
}

/// Lowercase a hostname for case-insensitive matching.
///
/// Returns `Cow::Borrowed` when the input is already lowercase (the common case),
/// avoiding allocation. Only allocates when uppercase ASCII bytes are present.
fn lowercase_hostname(host: &str) -> std::borrow::Cow<'_, str> {
    match host.bytes().any(|b| b.is_ascii_uppercase()) {
        false => std::borrow::Cow::Borrowed(host),
        true => std::borrow::Cow::Owned(host.to_ascii_lowercase()),
    }
}

impl FrozenHostRouter {
    /// Resolve a router from a host header value.
    fn resolve_host(&self, host_header: &str) -> Result<Option<&FrozenRouter>, Response> {
        match is_valid_host(host_header) {
            false => Err(Response::text_raw(400, "bad request")),
            true => {
                let hostname = strip_host_port(host_header);
                let lookup = lowercase_hostname(hostname);

                Ok(self
                    .hosts
                    .binary_search_by_key(&lookup.as_ref(), |(h, _)| h.as_ref())
                    .ok()
                    .map(|i| &self.hosts[i].1)
                    .or(self.default.as_ref()))
            }
        }
    }

    /// Find the matching FrozenRouter for a request's Host header.
    ///
    /// Returns `Err(Response)` with 400 if the Host header contains
    /// path separators or control characters.
    pub(super) fn resolve(&self, req: &Request) -> Result<Option<&FrozenRouter>, Response> {
        self.resolve_host(req.header("host").unwrap_or(""))
    }

    /// Find the matching FrozenRouter from borrowed request-head metadata.
    pub(super) fn resolve_from_head(
        &self,
        head: &RequestHead<'_>,
    ) -> Result<Option<&FrozenRouter>, Response> {
        self.resolve_host(head.header("host").unwrap_or(""))
    }
}
