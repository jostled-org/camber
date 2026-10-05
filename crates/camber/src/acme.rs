use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use rustls_acme::AcmeConfig as RustlsAcmeConfig;
use rustls_acme::caches::DirCache;

use crate::RuntimeError;
use crate::config::{AcmeBase, Challenge};
use crate::runtime_state::LifecycleSignals;
use crate::tls::{H2_ALPN, HTTP1_ALPN, server_config};

/// Re-export `AcmeState` so downstream crates don't need a direct `rustls-acme` dependency.
pub use rustls_acme::AcmeState;

/// Drive an ACME renewal event stream until it ends or a lifecycle signal
/// fires.
///
/// Generic over the stream so the loop the runtime admits is the same loop a
/// deterministic test drives with scripted events — the renewal stream itself
/// talks to a live ACME directory.
pub(crate) async fn acme_renewal_loop<S, T, E>(events: S, signals: LifecycleSignals)
where
    S: futures_util::Stream<Item = Result<T, E>>,
    T: std::fmt::Debug,
    E: std::fmt::Display,
{
    use futures_util::StreamExt;

    let mut events = std::pin::pin!(events);
    loop {
        // `guard` is the one definition of "race this work against the
        // lifecycle signals". The wait for the next renewal event is unbounded
        // — the stream is idle for weeks between renewals — so without it a
        // scope-owned loop would hold the drain open to its escalation
        // boundary.
        let event = match signals.guard(events.next()).await {
            ControlFlow::Break(()) => return,
            ControlFlow::Continue(event) => event,
        };
        match report_event(event) {
            ControlFlow::Break(()) => return,
            ControlFlow::Continue(()) => {}
        }
    }
}

/// Log one renewal event, and report whether the stream can still produce more.
///
/// An exhausted stream is not a lifecycle stop: nothing will drive provisioning
/// again, so the certificates the loop was renewing expire in place. The
/// lifecycle exit stays silent precisely so this one is distinguishable.
///
/// The event and the error are structured FIELDS, not part of the message.
/// Interpolating either would give one condition a distinct message string per
/// occurrence, which is the thing an operator filters on — and would make
/// `tracing` format a message per event that a field carries for free.
fn report_event<T, E>(event: Option<Result<T, E>>) -> ControlFlow<()>
where
    T: std::fmt::Debug,
    E: std::fmt::Display,
{
    match event {
        None => {
            tracing::warn!("acme: renewal stream ended; certificates will not be renewed");
            ControlFlow::Break(())
        }
        Some(Ok(ok)) => {
            tracing::info!(event = ?ok, "acme: renewal event");
            ControlFlow::Continue(())
        }
        Some(Err(err)) => {
            tracing::warn!(%err, "acme: renewal error");
            ControlFlow::Continue(())
        }
    }
}

/// Configuration for automatic TLS via ACME (Let's Encrypt) using TLS-ALPN-01 challenges.
///
/// Wraps [`AcmeBase`] with the TLS-ALPN-01-specific build step.
#[derive(Debug, Clone)]
pub struct AcmeConfig {
    base: AcmeBase,
}

impl AcmeConfig {
    /// Create a new ACME configuration for the given domains.
    ///
    /// `tool_name` sets the default cache directory to `~/.config/{tool_name}/certs/`.
    pub fn new(tool_name: &str, domains: impl IntoIterator<Item = impl Into<Box<str>>>) -> Self {
        Self {
            base: AcmeBase::new(tool_name, domains),
        }
    }

    /// Set the contact email for ACME registration.
    pub fn email(mut self, email: impl Into<Box<str>>) -> Self {
        self.base = self.base.email(email);
        self
    }

    /// Set the directory for caching certificates and account keys.
    pub fn cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.base = self.base.cache_dir(path);
        self
    }

    /// Use Let's Encrypt staging directory (for testing).
    pub fn staging(mut self, staging: bool) -> Self {
        self.base = self.base.staging(staging);
        self
    }

    /// Return the configured cache directory path.
    pub fn cache_path(&self) -> &std::path::Path {
        self.base.cache_path()
    }

    /// Validate the configured domain set for a TLS-ALPN-01 order.
    ///
    /// Performs no I/O. An empty set, a malformed name, an IP literal, a
    /// wildcard, or a name that repeats another after canonicalization returns
    /// `RuntimeError::Config`. Call it before any cache or listener effect;
    /// [`AcmeConfig::build`] repeats it.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.base.validated_domains(Challenge::TlsAlpn01).map(drop)
    }

    /// Build the rustls-acme state, returning the server config and renewal stream.
    ///
    /// The domain set is validated first, as [`AcmeConfig::validate`] does.
    ///
    /// The returned `AcmeState` is a `Stream` that must be polled to drive cert
    /// provisioning and renewal. Spawn it as a background Tokio task.
    pub fn build(
        self,
    ) -> Result<
        (
            Arc<rustls::ServerConfig>,
            rustls_acme::AcmeState<std::io::Error>,
        ),
        RuntimeError,
    > {
        let domains = self.base.validated_domains(Challenge::TlsAlpn01)?;
        // `RustlsAcmeConfig::new` takes `impl AsRef<str>` and copies each domain
        // into its own storage, so the canonical list is handed over by
        // reference.
        let AcmeBase {
            email,
            cache_dir,
            staging,
            ..
        } = self.base;

        let mut acme_cfg = RustlsAcmeConfig::new(domains.iter())
            .cache(DirCache::new(cache_dir))
            .directory_lets_encrypt(!staging);

        if let Some(email) = email {
            acme_cfg = acme_cfg.contact_push(format!("mailto:{email}"));
        }

        let state = acme_cfg.state();

        // ACME TLS-ALPN-01 challenge requires the acme-tls/1 ALPN token.
        // Also advertise h2 and http/1.1 for regular traffic.
        let server_config = server_config(
            state.resolver(),
            vec![
                rustls_acme::acme::ACME_TLS_ALPN_NAME.to_vec(),
                H2_ALPN.to_vec(),
                HTTP1_ALPN.to_vec(),
            ],
        )?;

        Ok((server_config, state))
    }
}
