pub use camber::config::TlsConfig;
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    listen: Option<Box<str>>,
    connection_limit: Option<usize>,
    tls: Option<TlsConfig>,
    #[serde(rename = "site")]
    sites: Vec<SiteConfig>,
}

#[derive(Debug, Deserialize)]
pub struct SiteConfig {
    host: Box<str>,
    proxy: Option<Box<str>>,
    root: Option<Box<str>>,
    health_check: Option<Box<str>>,
    health_interval: Option<u64>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let config: Config = camber::config::load_config(path).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.connection_limit == Some(0) {
            return Err("connection_limit must be at least 1".to_owned());
        }

        for site in &self.sites {
            if site.health_interval == Some(0) {
                return Err(format!(
                    "site \"{}\" health_interval must be at least 1",
                    site.host
                ));
            }

            match (&site.proxy, &site.root) {
                (None, None) => {
                    return Err(format!(
                        "site \"{}\" must have at least \"proxy\" or \"root\"",
                        site.host
                    ));
                }
                _ => {}
            }
        }

        if let Some(tls) = &self.tls {
            tls.validate().map_err(|e| e.to_string())?;
        }

        Ok(())
    }

    pub fn listen(&self) -> &str {
        self.listen.as_deref().unwrap_or("0.0.0.0:8080")
    }

    pub fn connection_limit(&self) -> Option<usize> {
        self.connection_limit
    }

    pub fn tls(&self) -> Option<&TlsConfig> {
        self.tls.as_ref()
    }

    pub fn sites(&self) -> &[SiteConfig] {
        &self.sites
    }

    /// Collect domain names from all site host fields.
    /// Used when auto-TLS is enabled to pass domains to the ACME provider.
    pub fn auto_tls_domains(&self) -> Box<[&str]> {
        self.sites
            .iter()
            .map(|s| s.host())
            .collect::<Vec<_>>()
            .into_boxed_slice()
    }
}

impl SiteConfig {
    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    pub fn health_check(&self) -> Option<&str> {
        self.health_check.as_deref()
    }

    pub fn health_interval(&self) -> Option<u64> {
        self.health_interval
    }
}
