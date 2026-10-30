use super::Response;
use super::body_admission::ConfiguredCeiling;
use super::dispatch::RouterEnvelope;
use super::policy_report::PolicyReport;
use super::policy_resolution::Located;
use super::policy_sources::{PolicyBound, PolicyLayer};
use super::rejection::{Rejected, Rejection, RejectionContext, RejectionMapper, shared_mapper};
use super::request::RequestHead;
use super::route_budgets::RouteBudgets;
use super::router::{FrozenRouter, Router};
use super::{BufferConfig, Request};
use crate::RuntimeError;
use std::sync::Arc;

impl std::fmt::Debug for HostRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostRouter")
            .field("host_count", &self.hosts.len())
            .field("has_default", &self.default.is_some())
            .field("buffers", &self.buffers)
            .field("has_rejection_mapper", &self.mapper.is_some())
            .field("body_ceiling", &self.body_ceiling)
            .finish()
    }
}

/// Build-time host-based router. Maps hostnames to Router instances.
#[derive(Default)]
pub struct HostRouter {
    hosts: Vec<(Box<str>, Router)>,
    default: Option<Router>,
    buffers: BufferConfig,
    mapper: Option<Arc<RejectionMapper>>,
    body_ceiling: ConfiguredCeiling,
    budgets: RouteBudgets,
}

impl HostRouter {
    /// Create an empty host router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the deadlines every request under this host router runs under.
    ///
    /// This budget contains every child router: a child that configures none
    /// inherits it, and a child that configures one can only narrow it.
    #[must_use]
    pub fn request_budget(mut self, budget: super::RequestBudget) -> Self {
        self.budgets = self.budgets.with_request(budget);
        self
    }

    /// Set the streaming-upload budget containing every child router.
    #[must_use]
    pub fn upload_budget(mut self, budget: super::TransferBudget) -> Self {
        self.budgets = self.budgets.with_upload(budget);
        self
    }

    /// Set the streaming-download budget containing every child router.
    #[must_use]
    pub fn download_budget(mut self, budget: super::TransferBudget) -> Self {
        self.budgets = self.budgets.with_download(budget);
        self
    }

    /// Set the requested maximum body size in bytes; zero admits empty bodies only.
    ///
    /// A child that configures no ceiling inherits this one. When both set
    /// one, the narrower applies. When only the child sets one, the child's
    /// applies: the host's 8 MiB default never narrows it. The resolved
    /// [`ServerPolicy`](super::ServerPolicy) hard cap always contains both.
    #[must_use]
    pub fn max_request_body(mut self, bytes: usize) -> Self {
        self.body_ceiling = ConfiguredCeiling::configured(bytes);
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

    /// Whether any host's router, or the default router, registers a route at
    /// exactly this static path.
    pub(super) fn claims_static_path(&self, path: &str) -> bool {
        self.hosts
            .iter()
            .map(|(_, router)| router)
            .chain(self.default.as_ref())
            .any(|router| router.claims_static_path(path))
    }

    /// Set the policy for refusals no child router claims.
    ///
    /// A resolved child router's own mapper wins. This one answers a malformed
    /// or unmatched Host, and every child that configured none.
    #[must_use]
    pub fn rejection_mapper<F>(mut self, mapper: F) -> Self
    where
        F: Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError>
            + Send
            + Sync
            + 'static,
    {
        self.mapper = Some(shared_mapper(mapper));
        self
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

    /// Freeze every child under this host router's resolved policy.
    ///
    /// `outer` is the server's resolved budgets and `cap` its request-body hard
    /// cap. Resolution runs once here, so routing only reads its results.
    pub(super) fn freeze(
        mut self,
        outer: Located<RouteBudgets>,
        cap: PolicyBound<usize>,
        report: &mut PolicyReport,
    ) -> FrozenHostRouter {
        let budgets = self.budgets.located(PolicyLayer::Host).contained_by(outer);
        report.budgets(PolicyLayer::Host, &self.budgets, &budgets);
        report.body_limit(
            PolicyLayer::Host,
            self.body_ceiling.configured_value(),
            self.body_ceiling.resolved_host(cap),
        );
        let fallback = RouterEnvelope {
            budgets,
            host_ceiling: Some(self.body_ceiling),
            cap,
            host: None,
        };
        self.hosts.sort_by(|(a, _), (b, _)| a.cmp(b));
        let hosts: Box<[(Box<str>, FrozenRouter)]> = self
            .hosts
            .into_iter()
            .map(|(host, router)| {
                let named = RouterEnvelope {
                    host: Some(&host),
                    ..fallback
                };
                let frozen = router.freeze(&named, report);
                (host, frozen)
            })
            .collect();
        let default = self.default.map(|router| router.freeze(&fallback, report));
        FrozenHostRouter {
            hosts,
            default,
            mapper: self.mapper,
            budgets: budgets.values(),
        }
    }
}

/// Immutable host-based router. Dispatches by Host header.
pub(super) struct FrozenHostRouter {
    hosts: Box<[(Box<str>, FrozenRouter)]>,
    default: Option<FrozenRouter>,
    mapper: Option<Arc<RejectionMapper>>,
    /// The resolved budgets a request runs under when no child claims it.
    budgets: RouteBudgets,
}

/// Reject values that are not an HTTP authority.
fn is_valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.contains('@')
        && host.parse::<hyper::http::uri::Authority>().is_ok()
        && has_valid_authority_port(host)
}

fn has_valid_authority_port(host: &str) -> bool {
    match host.strip_prefix('[') {
        Some(bracketed) => has_valid_bracketed_port(bracketed),
        None => has_valid_named_port(host),
    }
}

fn has_valid_bracketed_port(bracketed: &str) -> bool {
    match bracketed.split_once(']') {
        Some((_, "")) => true,
        Some((_, suffix)) => suffix
            .strip_prefix(':')
            .is_some_and(|port| port.parse::<u16>().is_ok()),
        None => false,
    }
}

fn has_valid_named_port(host: &str) -> bool {
    match host.split_once(':') {
        Some((hostname, port)) => {
            !hostname.is_empty() && !port.contains(':') && port.parse::<u16>().is_ok()
        }
        None => true,
    }
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
    /// The policy a request uses when its resolved child configured none.
    pub(super) fn mapper(&self) -> Option<Arc<RejectionMapper>> {
        self.mapper.clone()
    }

    /// The resolved budgets containing every child router here.
    pub(super) const fn budgets(&self) -> RouteBudgets {
        self.budgets
    }

    /// Resolve a router from the authority a request named.
    fn resolve_host(&self, authority: &str) -> Result<Option<&FrozenRouter>, Rejected> {
        match is_valid_host(authority) {
            false => Err(Rejected::invalid_host(authority)),
            true => Ok(self.claiming_router(authority)),
        }
    }

    /// The router that claims an authority already known to be one.
    fn claiming_router(&self, authority: &str) -> Option<&FrozenRouter> {
        let hostname = strip_host_port(authority);
        let lookup = lowercase_hostname(hostname);

        self.hosts
            .binary_search_by_key(&lookup.as_ref(), |(h, _)| h.as_ref())
            .ok()
            .map(|i| &self.hosts[i].1)
            .or(self.default.as_ref())
    }

    /// The router an authority selects, for a stage that answers without it.
    ///
    /// Scope selection asks only which mapper answers, and an authority that is
    /// not one selects no child — the same answer as an authority no child
    /// claims. Telling those two apart is what [`Self::resolve`] mints a
    /// refusal for, and a caller that needs neither dropped it unread: a
    /// `format!` detail and a shared allocation built and discarded on every
    /// malformed request, for a value nothing records.
    pub(super) fn router_for(&self, authority: &str) -> Option<&FrozenRouter> {
        match is_valid_host(authority) {
            false => None,
            true => self.claiming_router(authority),
        }
    }

    /// Find the matching FrozenRouter for a request's authority.
    ///
    /// Returns `Err` with a `Routing` refusal when the authority is not one:
    /// a value with path separators, control characters, or no host part at all.
    pub(super) fn resolve(&self, req: &Request) -> Result<Option<&FrozenRouter>, Rejected> {
        self.resolve_host(req.authority())
    }

    /// Find the matching FrozenRouter from borrowed request-head metadata.
    pub(super) fn resolve_from_head(
        &self,
        head: &RequestHead<'_>,
    ) -> Result<Option<&FrozenRouter>, Rejected> {
        self.resolve_host(head.authority())
    }
}
