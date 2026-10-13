//! The one factory every outbound transport is built by.
//!
//! A reusable transport and a pinned one are the same Reqwest client with the
//! same disabled behavior. A pinned one adds two settings: it uses no proxy,
//! and it dials its origin only at the addresses its operation checked.
//!
//! Each terminal has a timer profile. A buffered transport arms Reqwest's read
//! timer with the quiet interval. A streaming transport arms none, because the
//! owner of a streamed body measures its own demand. The two profiles are two
//! clients with two pools, and one write discards both.

use super::dns_edge::ClientDnsEdge;
use super::egress::EgressPolicy;
use super::error::ClientError;
use super::lifetime::Lifetime;
use super::pinned::PinnedDestination;
use super::projection::client_error_projection;
use crate::RuntimeError;
use reqwest::Url;
use std::sync::OnceLock;
use std::time::Duration;

/// A built client, or the category its build failed with.
///
/// The category is kept rather than the error because a cached failure is
/// handed to every later call. `reqwest::Error` does not clone, and its text
/// can repeat what the builder was given.
pub(super) type BuiltClient = Result<reqwest::Client, ClientError>;

/// Which terminal a transport serves, and so which timers it arms.
#[derive(Clone, Copy)]
pub(super) enum Profile {
    /// Reqwest's read timer enforces the quiet interval.
    Buffered,
    /// No read timer. The owner of the streamed body enforces demand idle.
    Streaming,
}

/// The reusable transports of one client: one for each profile.
///
/// Each is built on first use. A client replaces the whole cache when a
/// transport setting is written, so no write keeps either profile.
#[derive(Default)]
pub(super) struct TransportCache {
    buffered: OnceLock<BuiltClient>,
    streaming: OnceLock<BuiltClient>,
}

impl TransportCache {
    /// The transport of `profile`, built by `build` on first use.
    pub(super) fn get_or_build(
        &self,
        profile: Profile,
        build: impl FnOnce() -> BuiltClient,
    ) -> &BuiltClient {
        match profile {
            Profile::Buffered => &self.buffered,
            Profile::Streaming => &self.streaming,
        }
        .get_or_init(build)
    }

    /// Whether either profile holds a built transport.
    pub(super) fn is_built(&self) -> bool {
        self.buffered.get().is_some() || self.streaming.get().is_some()
    }
}

/// The client settings every transport of that client is built from.
pub(super) struct TransportSettings<'a> {
    pub(super) connect_timeout: Duration,
    /// The quiet interval, or `None` to arm no read timer.
    pub(super) idle: Option<Duration>,
    /// The one local trust root a test added. It adds trust and removes no
    /// check.
    pub(super) test_root: Option<&'a reqwest::Certificate>,
}

/// The transport one operation sends through.
pub(super) enum OperationTransport<'a> {
    /// The client's reusable transport, and the pool every unrestricted call
    /// of that client and profile shares.
    Reusable(&'a reqwest::Client),
    /// A handle to that same reusable transport, for a task that outlives the
    /// borrow of its client. The pool is the shared one.
    Held(reqwest::Client),
    /// A transport built for this operation and pinned to its checked
    /// addresses. Its pool is released when the operation ends.
    Pinned(reqwest::Client),
}

impl OperationTransport<'_> {
    pub(super) fn client(&self) -> &reqwest::Client {
        match self {
            Self::Reusable(shared) => shared,
            Self::Held(owned) | Self::Pinned(owned) => owned,
        }
    }
}

/// Resolve and check `target` under `lifetime`, and build the transport that
/// dials only its checked addresses.
///
/// Every restricted operation of either terminal gets its transport here. The
/// lookup wait ends with the operation, and a transport is built only for an
/// answer the policy admitted whole.
pub(super) async fn pinned_transport(
    policy: &EgressPolicy,
    edge: Option<&ClientDnsEdge>,
    settings: &TransportSettings<'_>,
    target: &Url,
    lifetime: &Lifetime,
) -> Result<reqwest::Client, RuntimeError> {
    let pinned = lifetime
        .within(PinnedDestination::resolve(policy, edge, target))
        .await?
        .map_err(RuntimeError::Client)?;
    build_client(settings, Some(&pinned)).map_err(RuntimeError::Client)
}

/// What a restricted streaming operation resolves, checks, and builds its
/// transport from, copied out of its client.
///
/// The operation's task outlives the borrow of the client, so it owns these
/// values. It arms no read timer.
pub(super) struct PinnedDialer {
    pub(super) policy: EgressPolicy,
    pub(super) edge: Option<ClientDnsEdge>,
    pub(super) connect_timeout: Duration,
    pub(super) test_root: Option<reqwest::Certificate>,
}

/// How one streaming operation gets its transport, fixed at its first poll.
pub(super) enum Dialer {
    /// The client's reusable streaming transport.
    Reusable(reqwest::Client),
    /// A transport pinned to the operation's own checked addresses.
    Pinned(PinnedDialer),
}

impl Dialer {
    /// The transport of one operation to `target`.
    pub(super) async fn open(
        self,
        target: &Url,
        lifetime: &Lifetime,
    ) -> Result<OperationTransport<'static>, RuntimeError> {
        match self {
            Self::Reusable(shared) => Ok(OperationTransport::Held(shared)),
            Self::Pinned(dialer) => {
                let settings = TransportSettings {
                    connect_timeout: dialer.connect_timeout,
                    idle: None,
                    test_root: dialer.test_root.as_ref(),
                };
                pinned_transport(
                    &dialer.policy,
                    dialer.edge.as_ref(),
                    &settings,
                    target,
                    lifetime,
                )
                .await
                .map(OperationTransport::Pinned)
            }
        }
    }
}

/// Build the Reqwest client `settings` describe, pinned when `pinned` is
/// given.
///
/// Reqwest enforces the connect bound and the quiet interval, and no other
/// timer. Its whole-request timeout is never set: the operation owns the
/// attempt timer, and a second timer on the same boundary would compete with
/// it to name the failure. An unbounded quiet interval configures no timer at
/// all rather than a very long one, and so does the streaming profile.
///
/// Reqwest's own retry policy is turned off. Its default resends a request
/// after a low-level protocol NACK, which would put a second sender of the same
/// request beside this module's coordinator: a request refused a replay
/// here could still be replayed there, under a budget no Camber setting names.
///
/// Reqwest's redirect following is turned off for the same reason. A followed
/// redirect is a second request, to a target the caller did not name, carrying
/// the first request's headers. The transport returns every redirect as the
/// answer it is, and the operation follows one only under the request's
/// `RedirectPolicy`. The automatic `Referer` goes with it, and no cookie store
/// is configured, so the transport adds no header that repeats where a request
/// came from or what an earlier answer set.
///
/// Automatic decoding is turned off for every codec Reqwest supports. Each one
/// is a Cargo feature another crate in the build can enable, and an enabled
/// codec would decode the body and remove the headers that describe it. The
/// explicit settings keep the returned bytes the bytes the peer sent.
///
/// A pinned transport uses no proxy, from the environment or from the system.
/// A proxy would resolve the target itself, at an address this process never
/// checked. Its resolver answers the pinned host with the pinned addresses, so
/// no second lookup supplies a dial candidate. The request URL keeps its
/// host, which is the name the `Host` header, the TLS server name, and
/// certificate verification read.
pub(super) fn build_client(
    settings: &TransportSettings<'_>,
    pinned: Option<&PinnedDestination>,
) -> BuiltClient {
    let builder = reqwest::Client::builder()
        .connect_timeout(settings.connect_timeout)
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .tls_certs_merge(settings.test_root.cloned());
    let builder = match settings.idle {
        Some(idle) => builder.read_timeout(idle),
        None => builder,
    };
    let builder = match pinned {
        Some(pinned) => builder
            .no_proxy()
            .resolve_to_addrs(pinned.host(), pinned.addresses()),
        None => builder,
    };
    builder
        .build()
        .map_err(|error| client_error_projection(&error))
}
