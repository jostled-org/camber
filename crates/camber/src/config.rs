use crate::RuntimeError;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::path::Path;

/// Shared TLS configuration parsed from TOML.
/// Used by all suspension-stack tools (Camber, Kingpin, Damper).
#[derive(Debug, Deserialize)]
pub struct TlsConfig {
    pub cert: Option<Box<str>>,
    pub key: Option<Box<str>>,
    pub auto: Option<bool>,
    pub email: Option<Box<str>>,
    pub staging: Option<bool>,
    pub cache_dir: Option<Box<str>>,
    pub dns_provider: Option<Box<str>>,
    pub dns_api_token_env: Option<Box<str>>,
    pub dns_api_token_file: Option<Box<str>>,
}

impl TlsConfig {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let is_auto = self.auto.unwrap_or(false);
        let has_cert = self.cert.is_some();
        let has_key = self.key.is_some();
        let has_email = self.email.is_some();

        match (is_auto, has_cert || has_key, has_email, has_cert, has_key) {
            (true, true, _, _, _) => Err(RuntimeError::Config(
                "tls: auto and cert/key are mutually exclusive".into(),
            )),
            (true, false, false, _, _) => Err(RuntimeError::Config(
                "tls: auto = true requires email".into(),
            )),
            (true, false, true, _, _) => self.validate_dns(),
            (false, true, _, true, true) => Ok(()),
            (false, true, _, _, _) => Err(RuntimeError::Config(
                "tls: both cert and key must be provided".into(),
            )),
            (false, false, _, _, _) if self.dns_provider.is_some() => Err(RuntimeError::Config(
                "tls: dns_provider requires auto = true".into(),
            )),
            (false, false, _, _, _) => Err(RuntimeError::Config(
                "tls: must specify either auto = true or cert/key paths".into(),
            )),
        }
    }

    fn validate_dns(&self) -> Result<(), RuntimeError> {
        let has_env = self.dns_api_token_env.is_some();
        let has_file = self.dns_api_token_file.is_some();

        match (self.dns_provider.is_some(), has_env, has_file) {
            (true, true, true) => Err(RuntimeError::Config(
                "tls: dns_api_token_env and dns_api_token_file are mutually exclusive".into(),
            )),
            (true, false, false) => Err(RuntimeError::Config(
                "tls: dns_provider requires dns_api_token_env or dns_api_token_file".into(),
            )),
            _ => Ok(()),
        }
    }

    pub fn auto(&self) -> bool {
        self.auto.unwrap_or(false)
    }

    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    pub fn staging(&self) -> bool {
        self.staging.unwrap_or(false)
    }

    pub fn cert(&self) -> Option<&str> {
        self.cert.as_deref()
    }

    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    pub fn cache_dir(&self) -> Option<&str> {
        self.cache_dir.as_deref()
    }

    pub fn dns_provider(&self) -> Option<&str> {
        self.dns_provider.as_deref()
    }

    pub fn dns_api_token_env(&self) -> Option<&str> {
        self.dns_api_token_env.as_deref()
    }

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

/// Shared ACME configuration fields used by both HTTP-01 and DNS-01 flows.
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
}

/// Load and parse a TOML configuration file into the given type.
pub fn load_config<T: DeserializeOwned>(path: &Path) -> Result<T, RuntimeError> {
    let contents = std::fs::read_to_string(path)?;
    toml::from_str(&contents)
        .map_err(|e| RuntimeError::Config(format!("failed to parse config: {e}").into()))
}
