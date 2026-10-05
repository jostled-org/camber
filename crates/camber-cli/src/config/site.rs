//! Site admission: one `[[site]]` table in, one validated site out.

use serde::Deserialize;

use super::authority::SiteAuthority;
use super::upstream::{check_health_path, check_proxy_url};

/// One `[[site]]` table exactly as the file spells it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawSite {
    host: Box<str>,
    proxy: Option<Box<str>>,
    root: Option<Box<str>>,
    health_check: Option<Box<str>>,
    health_interval: Option<u64>,
}

/// Per-site virtual host configuration.
///
/// Only [`super::Config::load`] builds one, so every site it returns has a
/// canonical host, a well-formed proxy and health check, and a readable root.
#[derive(Debug)]
pub struct SiteConfig {
    authority: SiteAuthority,
    proxy: Option<Box<str>>,
    root: Option<Box<str>>,
    health_check: Option<Box<str>>,
    health_interval: Option<u64>,
}

/// Admit every site, then refuse hosts that repeat after normalization.
pub(super) fn admit_sites(raw: Box<[RawSite]>) -> Result<Box<[SiteConfig]>, String> {
    match raw.is_empty() {
        true => Err("config must declare at least one [[site]]".to_owned()),
        false => {
            let sites = raw
                .into_iter()
                .map(RawSite::admit)
                .collect::<Result<Box<[SiteConfig]>, String>>()?;
            check_distinct(&sites)?;
            Ok(sites)
        }
    }
}

/// Refuse two sites that select the same hostname router.
///
/// Sorted borrows, as the ACME owner does: one pointer per site.
fn check_distinct(sites: &[SiteConfig]) -> Result<(), String> {
    let mut hosts: Box<[&str]> = sites.iter().map(SiteConfig::routing_host).collect();
    hosts.sort_unstable();
    match hosts.windows(2).find(|pair| pair[0] == pair[1]) {
        Some(pair) => Err(format!("site \"{}\" appears more than once", pair[0])),
        None => Ok(()),
    }
}

impl RawSite {
    fn admit(self) -> Result<SiteConfig, String> {
        let authority = SiteAuthority::parse(&self.host)
            .map_err(|reason| format!("site host {:?} {reason}", self.host))?;
        let site = SiteConfig {
            authority,
            proxy: self.proxy,
            root: self.root,
            health_check: self.health_check,
            health_interval: self.health_interval,
        };
        site.check_backend()?;
        site.check_health()?;
        site.check_root()?;
        Ok(site)
    }
}

impl SiteConfig {
    fn refusal(&self, reason: &str) -> String {
        format!("site \"{}\" {reason}", self.host())
    }

    fn check_backend(&self) -> Result<(), String> {
        match (self.proxy.as_deref(), &self.root) {
            (None, None) => Err(self.refusal("must have at least \"proxy\" or \"root\"")),
            (Some(proxy), _) => {
                check_proxy_url(proxy).map_err(|reason| self.refusal(&format!("proxy {reason}")))
            }
            (None, Some(_)) => Ok(()),
        }
    }

    fn check_health(&self) -> Result<(), String> {
        match (
            self.health_check.as_deref(),
            self.health_interval,
            self.proxy.is_some(),
        ) {
            (None, Some(_), _) => Err(self.refusal("health_interval requires health_check")),
            (Some(_), _, false) => Err(self.refusal("health_check requires proxy")),
            (_, Some(0), _) => Err(self.refusal("health_interval must be at least 1")),
            (Some(path), _, true) => check_health_path(path)
                .map_err(|reason| self.refusal(&format!("health_check {reason}"))),
            (None, None, _) => Ok(()),
        }
    }

    /// Refuse a root this process cannot list: missing, not a directory, or
    /// unreadable.
    fn check_root(&self) -> Result<(), String> {
        let Some(root) = self.root.as_deref() else {
            return Ok(());
        };
        std::fs::read_dir(root).map(drop).map_err(|error| {
            self.refusal(&format!(
                "root {root:?} is not a readable directory: {error}"
            ))
        })
    }

    /// Return the canonical configured authority, including its optional port.
    pub fn host(&self) -> &str {
        self.authority.as_str()
    }

    /// Return the hostname routing key. Configured ports do not select routers.
    pub fn routing_host(&self) -> &str {
        self.authority.routing_host()
    }

    /// The certificate name this site needs under automatic TLS: its DNS host
    /// without the port. `None` for an IP host.
    pub(super) fn certificate_name(&self) -> Option<&str> {
        self.authority.certificate_name()
    }

    /// Return the proxy upstream URL, if configured.
    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    /// Return the local static file root, if configured.
    pub fn root(&self) -> Option<&str> {
        self.root.as_deref()
    }

    /// Return the health check path, if configured.
    pub fn health_check(&self) -> Option<&str> {
        self.health_check.as_deref()
    }

    /// Return the health check interval in seconds, if configured.
    pub fn health_interval(&self) -> Option<u64> {
        self.health_interval
    }
}
