use camber::config::{TlsConfig, TlsMode};
use serde::Deserialize;
use std::path::Path;

mod authority;
mod site;
mod upstream;

pub use site::SiteConfig;

/// The proxy configuration file exactly as it spells itself.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    listen: Option<Box<str>>,
    connection_limit: Option<usize>,
    tls: Option<TlsConfig>,
    #[serde(rename = "site")]
    sites: Box<[site::RawSite]>,
}

/// Top-level proxy configuration loaded from TOML.
///
/// Only [`Config::load`] builds one, and it validates the whole file first.
#[derive(Debug)]
pub struct Config {
    listen: Option<Box<str>>,
    connection_limit: Option<usize>,
    tls: Option<TlsMode>,
    sites: Box<[SiteConfig]>,
}

impl Config {
    /// Load, parse, and validate a proxy config file.
    ///
    /// Reads the file and lists each site root. Nothing else: no secret is
    /// loaded, no upstream is probed, no certificate is requested, and no
    /// listener is bound. Unknown fields, invalid or duplicate site hosts,
    /// malformed proxy URLs and health checks, unreadable roots, an
    /// inconsistent TLS block, and certificate names the TLS mode cannot
    /// prove are all refused here.
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw: RawConfig = camber::config::load_config(path).map_err(|e| e.to_string())?;
        Self::admit(raw)
    }

    fn admit(raw: RawConfig) -> Result<Self, String> {
        if raw.connection_limit == Some(0) {
            return Err("connection_limit must be at least 1".to_owned());
        }
        let sites = site::admit_sites(raw.sites)?;
        let tls = raw
            .tls
            .as_ref()
            .map(TlsConfig::mode)
            .transpose()
            .map_err(|e| e.to_string())?;
        let config = Self {
            sites,
            listen: raw.listen,
            connection_limit: raw.connection_limit,
            tls,
        };
        config.check_certificate_names()?;
        Ok(config)
    }

    /// Hand the certificate names an automatic mode needs to the ACME owner
    /// for its challenge.
    fn check_certificate_names(&self) -> Result<(), String> {
        let checked = match &self.tls {
            None | Some(TlsMode::Manual { .. }) => return Ok(()),
            Some(TlsMode::TlsAlpn(_)) => {
                let names = self.dns_certificate_names()?;
                camber::acme::AcmeConfig::new("camber", names.iter().copied()).validate()
            }
            Some(TlsMode::Dns01 { .. }) => {
                let names = self.dns_certificate_names()?;
                camber::dns01::AcmeDns01::new("camber", names.iter().copied()).validate()
            }
        };
        checked.map_err(|e| e.to_string())
    }

    /// The automatic TLS certificate names. Refuses an IP site, which no
    /// ACME challenge certifies.
    fn dns_certificate_names(&self) -> Result<Box<[&str]>, String> {
        match self
            .sites
            .iter()
            .find(|site| site.certificate_name().is_none())
        {
            Some(site) => Err(format!(
                "site \"{}\" is an IP address; automatic TLS certifies DNS names only",
                site.host()
            )),
            None => Ok(self.auto_tls_domains()),
        }
    }

    /// Return the bind address for the proxy.
    ///
    /// Defaults to `0.0.0.0:8080` when not specified.
    pub fn listen(&self) -> &str {
        self.listen.as_deref().unwrap_or("0.0.0.0:8080")
    }

    /// Return the configured global connection limit.
    pub fn connection_limit(&self) -> Option<usize> {
        self.connection_limit
    }

    /// Return the TLS mode the validated `[tls]` block selects, if any.
    pub fn tls(&self) -> Option<&TlsMode> {
        self.tls.as_ref()
    }

    /// Return all configured sites.
    pub fn sites(&self) -> &[SiteConfig] {
        &self.sites
    }

    /// Collect the certificate names automatic TLS requests: each DNS site
    /// host without its port, in site order. Site admission refuses a
    /// repeated host, so each name appears once.
    pub fn auto_tls_domains(&self) -> Box<[&str]> {
        self.sites
            .iter()
            .filter_map(SiteConfig::certificate_name)
            .collect()
    }
}
