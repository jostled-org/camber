use std::path::PathBuf;
use std::sync::Arc;

use rustls_acme::AcmeConfig as RustlsAcmeConfig;
use rustls_acme::caches::DirCache;

use crate::RuntimeError;
use crate::config::AcmeBase;

/// Re-export `AcmeState` so downstream crates don't need a direct `rustls-acme` dependency.
pub use rustls_acme::AcmeState;

/// Configuration for automatic TLS via ACME (Let's Encrypt) using HTTP-01 challenges.
///
/// Wraps [`AcmeBase`] with the HTTP-01-specific build step.
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

    /// Build the rustls-acme state, returning the server config and renewal stream.
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
        let domain_strings: Vec<String> = self.base.domains.iter().map(|d| d.to_string()).collect();

        let mut acme_cfg = RustlsAcmeConfig::new(domain_strings)
            .cache(DirCache::new(self.base.cache_dir))
            .directory_lets_encrypt(!self.base.staging);

        if let Some(email) = &self.base.email {
            acme_cfg = acme_cfg.contact_push(format!("mailto:{email}"));
        }

        let state = acme_cfg.state();
        let resolver = state.resolver();

        let mut server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| {
            RuntimeError::Tls(format!("failed to configure TLS protocol versions: {e}").into())
        })?
        .with_no_client_auth()
        .with_cert_resolver(resolver);

        // ACME TLS-ALPN-01 challenge requires the acme-tls/1 ALPN token.
        // Also advertise h2 and http/1.1 for regular traffic.
        server_config.alpn_protocols = vec![
            rustls_acme::acme::ACME_TLS_ALPN_NAME.to_vec(),
            b"h2".to_vec(),
            b"http/1.1".to_vec(),
        ];

        Ok((Arc::new(server_config), state))
    }
}
