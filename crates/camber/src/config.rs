use crate::RuntimeError;
use crate::secret::SecretRef;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::path::Path;

#[cfg(any(feature = "acme", feature = "dns01"))]
mod acme_domains;
mod dns_name;

pub use dns_name::canonical_dns_name;
#[cfg(feature = "dns01")]
pub(crate) use dns_name::{WILDCARD_PREFIX, without_root};

#[cfg(any(feature = "acme", feature = "dns01"))]
pub(crate) use acme_domains::Challenge;

/// The only built-in DNS-01 provider name.
const CLOUDFLARE_PROVIDER: &str = "cloudflare";

/// Shared TLS configuration parsed from TOML.
/// Used by all suspension-stack tools (Camber, Kingpin, Damper).
///
/// Unknown input fields are refused at parse time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// Path to a PEM-encoded certificate file for manual TLS.
    pub cert: Option<Box<str>>,
    /// Path to a PEM-encoded private key file for manual TLS.
    pub key: Option<Box<str>>,
    /// Enable automatic certificate provisioning.
    pub auto: Option<bool>,
    /// Contact email for ACME registration.
    pub email: Option<Box<str>>,
    /// Use the ACME staging environment instead of production.
    pub staging: Option<bool>,
    /// Directory used to cache ACME account and certificate data.
    pub cache_dir: Option<Box<str>>,
    /// DNS provider name for DNS-01 challenges.
    pub dns_provider: Option<Box<str>>,
    /// Environment variable containing the DNS API token.
    pub dns_api_token_env: Option<Box<str>>,
    /// File containing the DNS API token.
    pub dns_api_token_file: Option<Box<str>>,
}

/// The TLS mode a valid [`TlsConfig`] selects, holding only the fields that
/// mode reads.
///
/// [`TlsConfig::mode`] is the only parser. A caller matches on the mode
/// instead of checking field combinations again.
#[derive(Debug, Clone)]
pub enum TlsMode {
    /// Serve a PEM certificate and private key from files.
    Manual {
        /// Path to the PEM-encoded certificate file.
        cert: Box<str>,
        /// Path to the PEM-encoded private key file.
        key: Box<str>,
    },
    /// Obtain certificates through ACME TLS-ALPN-01.
    TlsAlpn(AcmeSettings),
    /// Obtain certificates through ACME DNS-01 with the built-in Cloudflare
    /// provider.
    Dns01 {
        /// The ACME account inputs.
        acme: AcmeSettings,
        /// Where the Cloudflare API token is read from. Nothing is read
        /// until the caller loads it.
        token: SecretRef,
    },
}

/// The ACME inputs both automatic TLS modes share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcmeSettings {
    /// Contact email for ACME registration.
    pub email: Box<str>,
    /// Use the ACME staging environment instead of production.
    pub staging: bool,
    /// Directory used to cache ACME account and certificate data. `None`
    /// selects the tool's default cache directory.
    pub cache_dir: Option<Box<str>>,
}

impl TlsConfig {
    /// Validate that the configured TLS mode is internally consistent.
    ///
    /// Every field must apply to the selected mode, and `dns_provider` must
    /// name the built-in `"cloudflare"` provider exactly. Nothing is read or
    /// loaded: a caller validates before it resolves any secret.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.mode().map(|_| ())
    }

    /// Parse the configuration into the TLS mode it selects.
    ///
    /// Refuses exactly what [`TlsConfig::validate`] refuses, with the same
    /// diagnostics. Nothing is read or loaded: the DNS token stays a
    /// [`SecretRef`].
    pub fn mode(&self) -> Result<TlsMode, RuntimeError> {
        match self.auto() {
            true => self.automatic_mode(),
            false => self.manual_mode(),
        }
    }

    fn automatic_mode(&self) -> Result<TlsMode, RuntimeError> {
        match (
            self.cert.is_some() || self.key.is_some(),
            self.email.as_deref(),
        ) {
            (true, _) => Err(RuntimeError::Config(
                "tls: auto and cert/key are mutually exclusive".into(),
            )),
            (false, None) => Err(RuntimeError::Config(
                "tls: auto = true requires email".into(),
            )),
            (false, Some(email)) => self.acme_mode(email),
        }
    }

    /// Select DNS-01 when a token source is named, TLS-ALPN-01 otherwise.
    fn acme_mode(&self, email: &str) -> Result<TlsMode, RuntimeError> {
        let token = self.dns_token()?;
        let acme = AcmeSettings {
            email: email.into(),
            staging: self.staging(),
            cache_dir: self.cache_dir.clone(),
        };
        Ok(match token {
            Some(token) => TlsMode::Dns01 { acme, token },
            None => TlsMode::TlsAlpn(acme),
        })
    }

    fn manual_mode(&self) -> Result<TlsMode, RuntimeError> {
        match self.first_automatic_field() {
            Some(field) => Err(RuntimeError::Config(
                format!("tls: {field} requires auto = true").into(),
            )),
            None => self.cert_pair(),
        }
    }

    fn cert_pair(&self) -> Result<TlsMode, RuntimeError> {
        match (self.cert.as_deref(), self.key.as_deref()) {
            (Some(cert), Some(key)) => Ok(TlsMode::Manual {
                cert: cert.into(),
                key: key.into(),
            }),
            (Some(_), None) | (None, Some(_)) => Err(RuntimeError::Config(
                "tls: both cert and key must be provided".into(),
            )),
            (None, None) => Err(RuntimeError::Config(
                "tls: must specify either auto = true or cert/key paths".into(),
            )),
        }
    }

    /// The first set field that only automatic TLS reads.
    fn first_automatic_field(&self) -> Option<&'static str> {
        [
            ("email", self.email.is_some()),
            ("staging", self.staging.is_some()),
            ("cache_dir", self.cache_dir.is_some()),
            ("dns_provider", self.dns_provider.is_some()),
            ("dns_api_token_env", self.dns_api_token_env.is_some()),
            ("dns_api_token_file", self.dns_api_token_file.is_some()),
        ]
        .into_iter()
        .find_map(|(field, set)| set.then_some(field))
    }

    /// The DNS-01 token source, or `None` when no DNS field is set.
    fn dns_token(&self) -> Result<Option<SecretRef>, RuntimeError> {
        match (
            self.dns_provider.as_deref(),
            self.dns_api_token_env.as_deref(),
            self.dns_api_token_file.as_deref(),
        ) {
            (None, None, None) => Ok(None),
            (None, _, _) => Err(RuntimeError::Config(
                "tls: dns_api_token_env/dns_api_token_file requires dns_provider".into(),
            )),
            (Some(CLOUDFLARE_PROVIDER), Some(env), None) => Ok(Some(SecretRef::Env(env.into()))),
            (Some(CLOUDFLARE_PROVIDER), None, Some(file)) => Ok(Some(SecretRef::File(file.into()))),
            (Some(CLOUDFLARE_PROVIDER), Some(_), Some(_)) => Err(RuntimeError::Config(
                "tls: dns_api_token_env and dns_api_token_file are mutually exclusive".into(),
            )),
            (Some(CLOUDFLARE_PROVIDER), None, None) => Err(RuntimeError::Config(
                "tls: dns_provider requires dns_api_token_env or dns_api_token_file".into(),
            )),
            (Some(other), _, _) => Err(RuntimeError::Config(
                format!(
                    "tls: dns_provider {other:?} is not supported; \
                     the only built-in provider is {CLOUDFLARE_PROVIDER:?}"
                )
                .into(),
            )),
        }
    }

    /// Return whether automatic TLS is enabled.
    pub fn auto(&self) -> bool {
        self.auto.unwrap_or(false)
    }

    /// Return the configured ACME contact email.
    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    /// Return whether ACME staging mode is enabled.
    pub fn staging(&self) -> bool {
        self.staging.unwrap_or(false)
    }

    /// Return the configured certificate path for manual TLS.
    pub fn cert(&self) -> Option<&str> {
        self.cert.as_deref()
    }

    /// Return the configured private key path for manual TLS.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    /// Return the configured ACME cache directory.
    pub fn cache_dir(&self) -> Option<&str> {
        self.cache_dir.as_deref()
    }

    /// Return the configured DNS provider name.
    pub fn dns_provider(&self) -> Option<&str> {
        self.dns_provider.as_deref()
    }

    /// Return the environment variable name holding the DNS API token.
    pub fn dns_api_token_env(&self) -> Option<&str> {
        self.dns_api_token_env.as_deref()
    }

    /// Return the file path holding the DNS API token.
    pub fn dns_api_token_file(&self) -> Option<&str> {
        self.dns_api_token_file.as_deref()
    }
}

/// Return the default cache directory: `~/.config/{tool}/certs/`.
#[cfg(any(feature = "acme", feature = "dns01"))]
pub(crate) fn default_cache_dir(tool: &str) -> std::path::PathBuf {
    home_dir().join(".config").join(tool).join("certs")
}

#[cfg(any(feature = "acme", feature = "dns01"))]
pub(crate) fn home_dir() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Shared ACME configuration fields used by both TLS-ALPN-01 and DNS-01 flows.
#[cfg(any(feature = "acme", feature = "dns01"))]
#[derive(Debug, Clone)]
pub struct AcmeBase {
    pub(crate) domains: std::sync::Arc<[Box<str>]>,
    pub(crate) email: Option<Box<str>>,
    pub(crate) cache_dir: std::path::PathBuf,
    pub(crate) staging: bool,
}

#[cfg(any(feature = "acme", feature = "dns01"))]
impl AcmeBase {
    /// Create a new ACME base configuration.
    ///
    /// `tool_name` sets the default cache directory to `~/.config/{tool_name}/certs/`.
    pub fn new(tool_name: &str, domains: impl IntoIterator<Item = impl Into<Box<str>>>) -> Self {
        Self {
            domains: domains.into_iter().map(Into::into).collect(),
            email: None,
            cache_dir: default_cache_dir(tool_name),
            staging: false,
        }
    }

    /// Set the contact email for ACME registration.
    pub fn email(mut self, email: impl Into<Box<str>>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// Set the directory for caching certificates and account keys.
    pub fn cache_dir(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.cache_dir = path.into();
        self
    }

    /// Use Let's Encrypt staging directory (for testing).
    pub fn staging(mut self, staging: bool) -> Self {
        self.staging = staging;
        self
    }

    /// Return the configured cache directory path.
    pub fn cache_path(&self) -> &std::path::Path {
        &self.cache_dir
    }

    /// Validate the configured domains for `challenge` and return them in
    /// canonical form. Performs no I/O.
    pub(crate) fn validated_domains(
        &self,
        challenge: Challenge,
    ) -> Result<std::sync::Arc<[std::sync::Arc<str>]>, RuntimeError> {
        acme_domains::validate_domains(&self.domains, challenge)
    }
}

/// Load and parse a TOML configuration file into the given type.
pub fn load_config<T: DeserializeOwned>(path: &Path) -> Result<T, RuntimeError> {
    let contents = std::fs::read_to_string(path)?;
    toml::from_str(&contents)
        .map_err(|e| RuntimeError::Config(format!("failed to parse config: {e}").into()))
}
