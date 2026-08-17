//! The upstream one registered proxy route reaches, frozen with that route.
//!
//! A route freezes its [`ProxyPolicy`] into one owner: the Reqwest client the
//! policy's transport dimensions produced, the deadlines Camber enforces
//! itself, the buffered ceiling, and the two streaming budgets. Two routes
//! registered under equal policies on one router share that owner; two routes
//! under different policies never do, and no router shares one with another.
//!
//! There is no process-wide client, which is the whole point: one shared client
//! gave every proxy route in a process the same connect and request timeouts,
//! so a route that configured its own could not have them. Ownership frozen
//! with the graph is what makes a route's phase deadlines the route's.

use super::proxy_policy::ProxyPolicy;
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use std::sync::Arc;
use std::time::Duration;

/// Everything one registered proxy route froze about reaching its upstream.
///
/// Deliberately not `Clone`. Routes share one owner through an [`Arc`] handed
/// out at registration, so a second copy of a client — and of the connection
/// pool and timeouts under it — cannot appear after the graph is frozen.
pub(super) struct ProxyUpstream {
    /// The client this route's transport dimensions produced, or the account of
    /// why none could be built.
    ///
    /// Held as the failure rather than raised at registration, because
    /// registration has no caller to answer: the route exists, and the first
    /// request through it is told that the proxy could not send.
    client: Result<reqwest::Client, Arc<str>>,
    policy: ProxyPolicy,
}

/// The owner the policy-free forwarding entry point reaches its upstream
/// through.
///
/// Not the process-wide client this module exists to remove. That one served
/// every route whatever policy the route configured, so a route could not have
/// its own bounds. This one is reachable only from
/// [`proxy_forward`](super::proxy_forward), which takes a backend and a prefix
/// and no policy at all: the documented defaults are the only bounds it can
/// ever carry, so one owner for it is the whole population rather than a cache
/// two configurations share.
static DEFAULT_UPSTREAM: std::sync::LazyLock<ProxyUpstream> =
    std::sync::LazyLock::new(|| ProxyUpstream::frozen(ProxyPolicy::default()));

impl ProxyUpstream {
    /// The owner a forward under the documented default policy runs through.
    pub(super) fn defaults() -> &'static Self {
        &DEFAULT_UPSTREAM
    }

    /// Freeze one policy into the upstream owner its routes forward through.
    fn frozen(policy: ProxyPolicy) -> Self {
        Self {
            client: build_client(policy).map_err(|error| -> Arc<str> { error.to_string().into() }),
            policy,
        }
    }

    /// The client this route forwards through.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Http`] carrying the account of the build this
    /// route's policy could not complete.
    pub(super) fn client(&self) -> Result<&reqwest::Client, RuntimeError> {
        self.client
            .as_ref()
            .map_err(|error| RuntimeError::Http(Arc::clone(error)))
    }

    /// How long this route's request may run before a usable head arrives.
    pub(super) const fn request_timeout(&self) -> Duration {
        self.policy.request()
    }

    /// The longest quiet interval this route allows between upstream frames.
    pub(super) const fn upstream_idle(&self) -> Duration {
        self.policy.upstream_idle()
    }

    /// The buffered maximum this route collects an upstream answer under.
    pub(super) const fn buffered_limit(&self) -> Option<usize> {
        self.policy.buffered_response()
    }

    /// The budget this route's streaming upload runs under.
    pub(super) const fn upload(&self) -> TransferBudget {
        self.policy.upload()
    }

    /// The budget this route's streaming download runs under.
    pub(super) const fn download(&self) -> TransferBudget {
        self.policy.download()
    }
}

/// Build the Reqwest client one proxy policy's transport dimensions describe.
///
/// Only the connect deadline is Reqwest's, because only Reqwest owns the
/// establishment it bounds. The request total and the upstream quiet interval
/// are enforced by the stages that can name them — an expiry Reqwest raised
/// would arrive as one untyped timeout, and this plan's whole claim is that an
/// operator can tell a dead upstream from a slow one.
fn build_client(policy: ProxyPolicy) -> Result<reqwest::Client, RuntimeError> {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(policy.connect())
        .build()
        .map_err(|error| RuntimeError::Http(error.to_string().into()))
}

/// The upstream owners one router's proxy routes were frozen with.
///
/// Registration-time state on the graph itself, never a process-wide cache:
/// the map lives on the [`Router`](super::Router) being built, is consulted
/// only while `&mut Router` exists, and is frozen with everything else the
/// router owns. Nothing looks a client up per request.
#[derive(Default)]
pub(super) struct ProxyClients {
    /// The distinct owners this graph has frozen, keyed by the policy each was
    /// built from.
    frozen: Vec<(ProxyPolicy, Arc<ProxyUpstream>)>,
    /// The owner each registration took, in registration order.
    ///
    /// Kept apart from the keyed set because they answer different questions:
    /// the set says which owners exist, and this says which one each route got.
    /// Only the second can show that two routes under one policy were handed
    /// the same owner.
    handed_out: Vec<Arc<ProxyUpstream>>,
}

impl ProxyClients {
    /// The owner one registration under `policy` forwards through.
    ///
    /// A policy this graph already froze is shared; anything else is frozen
    /// now. The search is linear over a router's distinct proxy policies, which
    /// is registration-time work over a handful of values.
    pub(super) fn frozen(&mut self, policy: ProxyPolicy) -> Arc<ProxyUpstream> {
        let existing = self
            .frozen
            .iter()
            .find(|(frozen, _)| *frozen == policy)
            .map(|(_, upstream)| Arc::clone(upstream));
        let upstream = existing.unwrap_or_else(|| {
            let upstream = Arc::new(ProxyUpstream::frozen(policy));
            self.frozen.push((policy, Arc::clone(&upstream)));
            upstream
        });
        self.handed_out.push(Arc::clone(&upstream));
        upstream
    }

    /// The identity of the owner each registration took, in registration order.
    ///
    /// The address of the frozen owner itself, which is the only value that
    /// tells a shared owner from a second one holding equal configuration. Read
    /// by the contract that proves equal policies share inside one graph and
    /// that two graphs share nothing.
    pub(super) fn identities(&self) -> Box<[usize]> {
        self.handed_out
            .iter()
            .map(|upstream| Arc::as_ptr(upstream) as usize)
            .collect()
    }
}
