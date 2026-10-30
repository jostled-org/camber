//! Immutable startup reports from the production policy resolution.

use super::effective_budget::{EffectiveRequestBudget, EffectiveTransferBudget};
use super::policy_dimension::{DimensionSources, PolicyDimension};
use super::policy_resolution::{Located, PolicyValues};
use super::policy_sources::{PolicyBound, PolicyLayer, PolicyLayers};
use super::route_budgets::RouteBudgets;
use super::{HealthRoute, Method, ProxyPolicy, ServerPolicy};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// A configured or effective policy dimension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyValue {
    /// A finite duration, or an unbounded duration.
    Duration(Option<Duration>),
    /// A finite byte/count maximum, or an unbounded maximum.
    Bytes(Option<usize>),
    /// The health route's nonnumeric exposure choice.
    Health(HealthRoute),
}

/// Writes `30s`, `8388608`, `unbounded`, `/health`, or `disabled`.
impl fmt::Display for PolicyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duration(Some(duration)) => write!(f, "{duration:?}"),
            Self::Bytes(Some(count)) => write!(f, "{count}"),
            Self::Duration(None) | Self::Bytes(None) => f.write_str("unbounded"),
            Self::Health(route) => f.write_str(route.path().unwrap_or("disabled")),
        }
    }
}

/// An explicit setting that a containing bound narrowed.
///
/// The containing bound is an explicit outer setting, or a bound that always
/// applies: the request-body hard cap, or the runtime shutdown deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PolicyDiagnostic {
    layer: PolicyLayer,
    dimension: PolicyDimension,
    configured: PolicyValue,
    effective: PolicyValue,
    sources: PolicyLayers,
}

impl PolicyDiagnostic {
    /// The layer whose explicit choice was narrowed.
    pub const fn layer(self) -> PolicyLayer {
        self.layer
    }
    /// The narrowed dimension.
    pub const fn dimension(self) -> PolicyDimension {
        self.dimension
    }
    /// The requested local value.
    pub const fn configured(self) -> PolicyValue {
        self.configured
    }
    /// The value enforced after containment.
    pub const fn effective(self) -> PolicyValue {
        self.effective
    }
    /// Every layer tied at the effective bound.
    pub const fn sources(self) -> PolicyLayers {
        self.sources
    }

    /// A diagnostic when `effective` differs from `configured`.
    pub(super) fn changed(
        layer: PolicyLayer,
        dimension: PolicyDimension,
        configured: PolicyValue,
        effective: PolicyValue,
        sources: PolicyLayers,
    ) -> Option<Self> {
        (configured != effective).then_some(Self {
            layer,
            dimension,
            configured,
            effective,
            sources,
        })
    }

    /// A diagnostic when an explicit byte limit resolved to a smaller one.
    fn body_limit(
        layer: PolicyLayer,
        configured: Option<usize>,
        effective: PolicyBound<usize>,
    ) -> Option<Self> {
        Self::changed(
            layer,
            PolicyDimension::RequestBodyLimit,
            PolicyValue::Bytes(Some(configured?)),
            PolicyValue::Bytes(Some(effective.value())),
            effective.sources(),
        )
    }
}

/// The server's frozen envelope and shutdown configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveServerPolicy {
    policy: Located<ServerPolicy>,
    runtime_shutdown: Option<Duration>,
    local_shutdown: Duration,
}

impl EffectiveServerPolicy {
    /// Resolve `local` inside the runtime policy `outer`, when one contains it.
    pub(super) fn resolve(local: ServerPolicy, outer: Option<ServerPolicy>) -> Self {
        let located = local.located(PolicyLayer::Server);
        let policy = match outer {
            Some(outer) => located.contained_by(outer.located(PolicyLayer::Runtime)),
            None => located,
        };
        Self {
            policy,
            runtime_shutdown: outer.map(|policy| policy.shutdown_timeout_value()),
            local_shutdown: local.shutdown_timeout_value(),
        }
    }

    /// The resolved values production owners enforce.
    pub(super) const fn policy(&self) -> ServerPolicy {
        self.policy.values()
    }

    /// The request and transfer budgets that contain every route.
    pub(super) fn route_budgets(&self) -> Located<RouteBudgets> {
        Located::new(
            self.policy().route_budgets(),
            DimensionSources::collect(RouteBudgets::dimensions(), |dimension| {
                self.policy.sources(dimension)
            }),
        )
    }

    /// One diagnostic for each explicit `local` value this resolution narrowed.
    pub(super) fn diagnostics(&self, local: &ServerPolicy) -> Vec<PolicyDiagnostic> {
        self.policy
            .diagnostics(PolicyLayer::Server, local)
            .collect()
    }

    fn bound<T: Copy>(&self, value: T, dimension: PolicyDimension) -> PolicyBound<T> {
        PolicyBound::new(value, self.policy.sources(dimension))
    }

    /// The request deadlines enforced by this server.
    pub fn request_budget(&self) -> EffectiveRequestBudget {
        EffectiveRequestBudget::of(&self.route_budgets())
    }
    /// The finite ceiling containing every configured request-body limit.
    pub fn request_body_hard_cap(&self) -> PolicyBound<usize> {
        self.bound(
            self.policy().request_body_hard_cap_value(),
            PolicyDimension::RequestBodyHardCap,
        )
    }
    /// The upload limits enforced by this server.
    pub fn upload_budget(&self) -> EffectiveTransferBudget {
        EffectiveTransferBudget::upload(&self.route_budgets())
    }
    /// The download limits enforced by this server.
    pub fn download_budget(&self) -> EffectiveTransferBudget {
        EffectiveTransferBudget::download(&self.route_budgets())
    }
    /// The pre-head timeout.
    pub fn header_timeout(&self) -> PolicyBound<Duration> {
        self.bound(
            self.policy().header_timeout_value(),
            PolicyDimension::HeaderTimeout,
        )
    }
    /// The effective server drain timeout.
    ///
    /// A contained server never drains past its runtime's shutdown deadline.
    pub fn shutdown_timeout(&self) -> PolicyBound<Duration> {
        self.bound(
            self.policy().shutdown_timeout_value(),
            PolicyDimension::ShutdownTimeout,
        )
    }
    /// The runtime aggregate grace, when a configured runtime contains this server.
    pub const fn runtime_shutdown_timeout(&self) -> Option<Duration> {
        self.runtime_shutdown
    }
    /// The server's own drain limit, configured or default.
    pub const fn local_shutdown_timeout(&self) -> Duration {
        self.local_shutdown
    }
    /// The connection admission limit.
    pub fn connection_limit(&self) -> PolicyBound<Option<usize>> {
        self.bound(
            self.policy().connection_limit_value(),
            PolicyDimension::ConnectionLimit,
        )
    }
    /// Health-route resolution under its exposure rules.
    pub fn health_route(&self) -> PolicyBound<HealthRoute> {
        self.bound(
            self.policy().health_route_value(),
            PolicyDimension::HealthRoute,
        )
    }
    /// The profiling response maximum.
    #[cfg(feature = "profiling")]
    pub fn profiling_response_limit(&self) -> PolicyBound<Option<usize>> {
        self.bound(
            super::server_policy::frozen_profiling_response_limit(&self.policy()),
            PolicyDimension::ProfilingResponseLimit,
        )
    }

    /// Whether both reports enforce the same values, whatever their sources.
    pub fn same_bounds(&self, other: &Self) -> bool {
        let (ours, theirs) = (self.policy(), other.policy());
        ServerPolicy::dimensions().all(|dimension| ours.value(dimension) == theirs.value(dimension))
            && self.runtime_shutdown == other.runtime_shutdown
            && self.local_shutdown == other.local_shutdown
    }
}

/// One registered route's configured effective envelope.
///
/// A handler-created response budget and a request-dependent admission policy
/// may narrow this envelope after startup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveRoutePolicy {
    host: Option<Arc<str>>,
    method: Method,
    route: Arc<str>,
    budgets: Located<RouteBudgets>,
    proxy: Option<EffectiveProxyPolicy>,
    body: Option<PolicyBound<usize>>,
    configured_body: Option<usize>,
}

/// What a proxy registration does with an upstream response body.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BufferedResponseLimit {
    /// The registration streams the response and buffers none of it.
    Streamed,
    /// The registration buffers at most this many bytes.
    Bytes(usize),
    /// The registration buffers the response with no ceiling.
    Unbounded,
}

/// The upstream-specific bounds a proxy registration actually enforces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveProxyPolicy {
    policy: ProxyPolicy,
    streaming: bool,
}

impl EffectiveProxyPolicy {
    pub(super) const fn new(policy: ProxyPolicy, streaming: bool) -> Self {
        Self { policy, streaming }
    }
    /// Time allowed to establish the upstream transport.
    pub const fn connect_timeout(self) -> Duration {
        self.policy.connect()
    }
    /// Time allowed until the upstream response head.
    pub const fn request_timeout(self) -> Duration {
        self.policy.request()
    }
    /// Maximum quiet interval between upstream body frames.
    pub const fn upstream_idle_timeout(self) -> Duration {
        self.policy.upstream_idle()
    }
    /// How much of an upstream response this registration buffers.
    pub const fn buffered_response_limit(self) -> BufferedResponseLimit {
        match (self.streaming, self.policy.buffered_response()) {
            (true, _) => BufferedResponseLimit::Streamed,
            (false, Some(bytes)) => BufferedResponseLimit::Bytes(bytes),
            (false, None) => BufferedResponseLimit::Unbounded,
        }
    }
}

impl EffectiveRoutePolicy {
    pub(super) fn new(
        host: Option<Arc<str>>,
        method: Method,
        route: Arc<str>,
        budgets: Located<RouteBudgets>,
        proxy: Option<EffectiveProxyPolicy>,
    ) -> Self {
        Self {
            host,
            method,
            route,
            budgets,
            proxy,
            body: None,
            configured_body: None,
        }
    }
    pub(super) fn with_body(
        mut self,
        configured: Option<usize>,
        body: Option<PolicyBound<usize>>,
    ) -> Self {
        self.configured_body = configured;
        self.body = body;
        self
    }
    /// The router's requested ceiling before containing policies narrow it.
    /// `None` means the router inherited its host or used the 8 MiB default.
    pub const fn configured_request_body_limit(&self) -> Option<usize> {
        self.configured_body
    }
    /// The configured admission ceiling, or `None` for a bodyless registration.
    /// A request-dependent admission callback may narrow it further.
    pub const fn request_body_limit(&self) -> Option<PolicyBound<usize>> {
        self.body
    }
    /// The configured host, or `None` for a standalone or fallback router.
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }
    /// The registered method. Implicit HEAD fallback is not a separate registration.
    pub const fn method(&self) -> Method {
        self.method
    }
    /// The normalized pattern stored by the routing trie.
    pub fn route(&self) -> &str {
        &self.route
    }
    /// Effective request deadlines for this registration.
    pub fn request_budget(&self) -> EffectiveRequestBudget {
        EffectiveRequestBudget::of(&self.budgets)
    }
    /// Upload envelope where this registration creates a streaming-transfer owner.
    /// Multipart and streaming proxy uploads report deadlines only:
    /// [`Self::request_body_limit`] supplies their payload-byte ceiling.
    pub fn upload_budget(&self) -> EffectiveTransferBudget {
        EffectiveTransferBudget::upload(&self.budgets)
    }
    /// Download envelope where this registration creates a streaming-transfer owner.
    /// Buffered proxy response limits are exposed through [`Self::proxy`].
    pub fn download_budget(&self) -> EffectiveTransferBudget {
        EffectiveTransferBudget::download(&self.budgets)
    }

    /// Upstream-specific limits, when this registration proxies requests.
    pub const fn proxy(&self) -> Option<EffectiveProxyPolicy> {
        self.proxy
    }

    /// Whether both reports enforce the same values, whatever their sources.
    pub fn same_bounds(&self, other: &Self) -> bool {
        let (ours, theirs) = (self.budgets.values(), other.budgets.values());
        self.host == other.host
            && self.method == other.method
            && self.route == other.route
            && RouteBudgets::dimensions()
                .all(|dimension| ours.value(dimension) == theirs.value(dimension))
            && self.proxy == other.proxy
            && self.body.map(PolicyBound::value) == other.body.map(PolicyBound::value)
            && self.configured_body == other.configured_body
    }
}

/// The exact policy snapshot validated before this server starts accepting.
///
/// Equality compares the whole report, sources and diagnostics included.
/// [`Self::same_bounds`] compares only the enforced values.
///
/// One shared allocation: a clone is a reference-count increment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectivePolicies(Arc<FrozenReport>);

/// The immutable contents every clone of one report shares.
#[derive(Debug, Eq, PartialEq)]
struct FrozenReport {
    server: EffectiveServerPolicy,
    routes: Box<[EffectiveRoutePolicy]>,
    diagnostics: Box<[PolicyDiagnostic]>,
}

impl EffectivePolicies {
    /// The effective server-wide envelope.
    pub fn server(&self) -> &EffectiveServerPolicy {
        &self.0.server
    }
    /// Actual registrations in the frozen routing trie.
    pub fn routes(&self) -> &[EffectiveRoutePolicy] {
        &self.0.routes
    }
    /// Explicit values narrowed by containing settings.
    pub fn diagnostics(&self) -> &[PolicyDiagnostic] {
        &self.0.diagnostics
    }

    /// Whether both reports enforce the same values, whatever their sources.
    pub fn same_bounds(&self, other: &Self) -> bool {
        self.server().same_bounds(other.server())
            && self.routes().len() == other.routes().len()
            && self
                .routes()
                .iter()
                .zip(other.routes())
                .all(|(ours, theirs)| ours.same_bounds(theirs))
    }

    pub(super) fn emit_diagnostics(&self) {
        self.diagnostics().iter().for_each(|diagnostic| {
            tracing::warn!(
                name: "camber.http.policy.narrowed",
                layer = %diagnostic.layer,
                dimension = %diagnostic.dimension,
                configured = %diagnostic.configured,
                effective = %diagnostic.effective,
                limiting_layers = %diagnostic.sources,
                "explicit policy setting narrowed"
            )
        });
    }
}

/// The report one terminal `serve*` call builds while it freezes routing.
pub(super) struct PolicyReport {
    server: EffectiveServerPolicy,
    routes: Vec<EffectiveRoutePolicy>,
    diagnostics: Vec<PolicyDiagnostic>,
}

impl PolicyReport {
    /// Start a report with the server's own diagnostics.
    pub(super) fn new(server: EffectiveServerPolicy, local: &ServerPolicy) -> Self {
        Self {
            server,
            routes: Vec::new(),
            diagnostics: server.diagnostics(local),
        }
    }

    /// The server envelope routing resolves inside.
    pub(super) const fn server(&self) -> &EffectiveServerPolicy {
        &self.server
    }

    /// Record the diagnostics one layer's budget resolution produced.
    pub(super) fn budgets(
        &mut self,
        layer: PolicyLayer,
        local: &RouteBudgets,
        effective: &Located<RouteBudgets>,
    ) {
        self.diagnostics.extend(effective.diagnostics(layer, local));
    }

    /// Record a diagnostic when an explicit byte limit resolved to a smaller one.
    pub(super) fn body_limit(
        &mut self,
        layer: PolicyLayer,
        configured: Option<usize>,
        effective: PolicyBound<usize>,
    ) {
        self.diagnostics
            .extend(PolicyDiagnostic::body_limit(layer, configured, effective));
    }

    /// Record one stored registration.
    pub(super) fn route(&mut self, route: EffectiveRoutePolicy) {
        self.routes.push(route);
    }

    /// The immutable report.
    pub(super) fn finish(self) -> EffectivePolicies {
        EffectivePolicies(Arc::new(FrozenReport {
            server: self.server,
            routes: self.routes.into_boxed_slice(),
            diagnostics: self.diagnostics.into_boxed_slice(),
        }))
    }
}
