//! The checked addresses one restricted operation may dial.
//!
//! A restricted operation resolves its target once. Every address of that
//! answer is read by the policy before any is dialed, and one refused address
//! refuses the whole answer. The addresses that remain are frozen here and
//! become the transport's only dial candidates, so the address that was
//! checked is the address that is connected to.

use super::dns_edge::ClientDnsEdge;
use super::egress::{EgressPolicy, literal_address};
use super::error::{ClientEgress, ClientError};
use reqwest::Url;
use std::net::{IpAddr, SocketAddr};

/// The most addresses one answer may hold.
///
/// An answer is read one address past this bound, so a longer answer is
/// refused and never truncated.
const VISIT_BOUND: usize = 64;

/// The refusal of a target that names no host or no port to dial.
///
/// No described request reaches it: a request's target is an HTTP or HTTPS
/// URL with a host.
const UNDIALABLE: ClientError = ClientError::Egress(ClientEgress::Scheme);

/// One origin's host, and the only addresses it may be dialed at.
///
/// Uniquely owned by one operation and frozen when it is made. Nothing is
/// added to it, and no later lookup replaces it.
pub(super) struct PinnedDestination {
    /// The host as the request URL writes it, which is the name the transport
    /// asks its resolver for.
    host: Box<str>,
    /// Canonical, without duplicates, at the origin's effective port.
    addresses: Box<[SocketAddr]>,
}

impl PinnedDestination {
    /// Resolve `target` once, and freeze its answer when `policy` admits
    /// every address of it.
    ///
    /// A literal is its own answer and starts no lookup. `edge` answers a
    /// name's lookup in place of the system resolver. An empty or failed
    /// lookup returns `Dns`.
    pub(super) async fn resolve(
        policy: &EgressPolicy,
        edge: Option<&ClientDnsEdge>,
        target: &Url,
    ) -> Result<Self, ClientError> {
        let host = target.host_str().ok_or(UNDIALABLE)?;
        let port = target.port_or_known_default().ok_or(UNDIALABLE)?;
        let candidates = match (literal_address(target), edge) {
            (Some(literal), _) => Box::from([literal]),
            (None, Some(edge)) => edge.lookup().await.ok_or(ClientError::Dns)?,
            (None, None) => system_lookup(host, port).await?,
        };
        Ok(Self {
            host: host.into(),
            addresses: approved(policy, target, port, &candidates)?,
        })
    }

    pub(super) fn host(&self) -> &str {
        &self.host
    }

    pub(super) fn addresses(&self) -> &[SocketAddr] {
        &self.addresses
    }
}

/// Ask the system resolver for `host` once.
///
/// The answer is read one address past the visit bound and no further. The
/// lookup runs on a blocking thread that can outlive a cancelled wait. That
/// thread owns only the name and its answer.
async fn system_lookup(host: &str, port: u16) -> Result<Box<[IpAddr]>, ClientError> {
    tokio::net::lookup_host((host, port))
        .await
        .map(|found| {
            found
                .take(VISIT_BOUND + 1)
                .map(|address| address.ip())
                .collect()
        })
        .map_err(|_| ClientError::Dns)
}

/// The addresses of one answer, when `policy` admits all of them.
///
/// The visit bound is read before any duplicate is removed, so a repeated
/// address counts each time it was returned. One refused address refuses the
/// answer: the rest are not dialed in its place.
fn approved(
    policy: &EgressPolicy,
    target: &Url,
    port: u16,
    candidates: &[IpAddr],
) -> Result<Box<[SocketAddr]>, ClientError> {
    let visited = match candidates.len() {
        0 => Err(ClientError::Dns),
        1..=VISIT_BOUND => Ok(candidates),
        _ => Err(ClientError::Egress(ClientEgress::ResolutionLimit)),
    }?;
    policy
        .decide_each(target, visited)
        .map_err(ClientError::Egress)?;
    Ok(frozen(visited, port))
}

/// `visited` as dial candidates: canonical, in order, each address once.
fn frozen(visited: &[IpAddr], port: u16) -> Box<[SocketAddr]> {
    visited
        .iter()
        .map(|address| SocketAddr::new(address.to_canonical(), port))
        .fold(Vec::with_capacity(visited.len()), |mut kept, address| {
            if !kept.contains(&address) {
                kept.push(address);
            }
            kept
        })
        .into_boxed_slice()
}
