//! The built-in Cloudflare DNS provider.
//!
//! A [`CloudflareProvider`] is an inert descriptor: a validated token and API
//! base. Constructing or dropping one sends nothing and claims nothing.
//! [`DnsProvider::prepare`] resolves each configured domain's own zone and
//! publishes the complete map only after every lookup succeeded. Creates and
//! deletes then reach only that prepared authority, by exact name and exact
//! record ID.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Deserialize;
use serde::Serialize;
use serde::de::IgnoredAny;

use super::cloudflare_wire::{answer, required};
use super::failure::{failure, invalid_config, outcome_unknown, rejected};
use super::provider::{DnsProvider, RecordId, challenge_name};
use super::transport::anonymous;
use crate::config::{WILDCARD_PREFIX, without_root};
use crate::error::Effect;
use crate::integration_lifecycle::integration;
use crate::runtime_state::recover_poisoned;
use crate::{
    IntegrationError, IntegrationFailure, IntegrationOperation, Retryability, RuntimeError,
};

const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

/// How long a challenge record lives, in seconds.
const RECORD_TTL: u32 = 120;

/// Cloudflare DNS provider for ACME DNS-01 challenges.
pub struct CloudflareProvider {
    api_token: Box<str>,
    base_url: Box<str>,
    /// The authority the last successful preparation published.
    authority: Option<ZoneAuthority>,
}

/// One prepared order's authority: which zone owns each challenge name, and
/// which records this authority created.
struct ZoneAuthority {
    client: reqwest::Client,
    /// Each prepared domain's challenge name, and the zone that owns it.
    zones: Box<[(Box<str>, Arc<str>)]>,
    /// Records created here and not yet deleted, with their zones.
    records: Mutex<Vec<(RecordId, Arc<str>)>>,
}

impl CloudflareProvider {
    /// A descriptor for the production Cloudflare API.
    ///
    /// Performs no I/O.
    ///
    /// # Errors
    ///
    /// `ZoneLookup` with `InvalidConfig` for an empty token or one with
    /// whitespace or control characters.
    pub fn new(api_token: Box<str>) -> Result<Self, RuntimeError> {
        Self::with_base_url(api_token, CLOUDFLARE_API.into())
    }

    /// A descriptor for the Cloudflare-shaped API at `base_url`.
    ///
    /// Performs no I/O. The base must be an absolute `https` URL, or `http`
    /// to a loopback host, with no credentials, query, or fragment.
    ///
    /// # Errors
    ///
    /// `ZoneLookup` with `InvalidConfig` for an invalid token or base.
    pub fn with_base_url(api_token: Box<str>, base_url: Box<str>) -> Result<Self, RuntimeError> {
        match (valid_token(&api_token), valid_base(&base_url)) {
            (true, true) => Ok(Self {
                api_token,
                base_url: base_url.trim_end_matches('/').into(),
                authority: None,
            }),
            _ => Err(integration(invalid_config(
                IntegrationOperation::ZoneLookup,
            ))),
        }
    }

    /// The authority the last preparation published, or a refusal of
    /// `operation` before anything is sent.
    fn prepared(
        &self,
        operation: IntegrationOperation,
    ) -> Result<&ZoneAuthority, IntegrationError> {
        self.authority.as_ref().ok_or(rejected(operation))
    }

    /// The zone that owns `domain`: the longest suffix for which Cloudflare
    /// answers exactly one zone of that name with a nonempty ID.
    ///
    /// An empty answer tries the next suffix. Anything else ends the lookup
    /// rather than fall back to a broader zone.
    async fn lookup_zone(
        &self,
        client: &reqwest::Client,
        zones_url: &str,
        domain: &str,
    ) -> Result<Arc<str>, IntegrationError> {
        let mut candidate = domain;
        while candidate.contains('.') {
            let request = client
                .get(zones_url)
                .bearer_auth(&*self.api_token)
                .query(&[("name", candidate)]);
            let zones: Vec<CfZone> = required(
                answer(request, IntegrationOperation::ZoneLookup, Effect::ReadOnly).await?,
                IntegrationOperation::ZoneLookup,
                Effect::ReadOnly,
            )?;
            match zones.as_slice() {
                [] => {}
                [zone] if zone.names(candidate) => return Ok(Arc::from(&*zone.id)),
                _ => return Err(rejected(IntegrationOperation::ZoneLookup)),
            }
            candidate = candidate.split_once('.').map_or("", |(_, parent)| parent);
        }
        Err(rejected(IntegrationOperation::ZoneLookup))
    }
}

impl DnsProvider for CloudflareProvider {
    async fn prepare(&mut self, domains: &[Arc<str>]) -> Result<(), RuntimeError> {
        self.authority = None;
        let client = super::transport::client_builder(&[])
            .and_then(reqwest::ClientBuilder::build)
            .map_err(|error| {
                integration(
                    failure(
                        IntegrationOperation::ZoneLookup,
                        IntegrationFailure::Unavailable,
                        Retryability::Safe,
                    )
                    .with_source(Arc::new(error)),
                )
            })?;
        let zones_url = format!("{}/zones", self.base_url);
        let mut zones: Vec<(Box<str>, Arc<str>)> = Vec::with_capacity(domains.len());
        for domain in domains {
            let subject = zone_subject(domain);
            let name = challenge_name(subject);
            // A wildcard and its base share one challenge name, so one lookup.
            if zones
                .iter()
                .any(|(prepared, _)| prepared.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            let zone = self
                .lookup_zone(&client, &zones_url, subject)
                .await
                .map_err(integration)?;
            zones.push((name, zone));
        }
        self.authority = Some(ZoneAuthority {
            client,
            zones: zones.into_boxed_slice(),
            records: Mutex::new(Vec::new()),
        });
        Ok(())
    }

    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<RecordId, RuntimeError> {
        let operation = IntegrationOperation::CreateTxt;
        let authority = self.prepared(operation).map_err(integration)?;
        let zone = authority
            .zone_of_name(fqdn)
            .ok_or(rejected(operation))
            .map_err(integration)?;
        let request = authority
            .client
            .post(format!("{}/zones/{zone}/dns_records", self.base_url))
            .bearer_auth(&*self.api_token)
            .json(&CreateRecord {
                r#type: "TXT",
                name: fqdn,
                content: value,
                ttl: RECORD_TTL,
            });
        let created = answer::<CfRecord>(request, operation, Effect::SideEffect)
            .await
            .and_then(|record| required(record, operation, Effect::SideEffect))
            .and_then(|record| record.acknowledged(operation))
            .map_err(integration)?;
        authority.remember(created.clone(), zone);
        Ok(created)
    }

    async fn delete_txt_record(&self, record_id: &str) -> Result<(), RuntimeError> {
        let operation = IntegrationOperation::DeleteTxt;
        let authority = self.prepared(operation).map_err(integration)?;
        let zone = authority
            .zone_of_record(record_id)
            .ok_or(rejected(operation))
            .map_err(integration)?;
        let request = authority
            .client
            .delete(format!(
                "{}/zones/{zone}/dns_records/{record_id}",
                self.base_url
            ))
            .bearer_auth(&*self.api_token);
        answer::<IgnoredAny>(request, operation, Effect::SideEffect)
            .await
            .map_err(integration)?;
        authority.forget(record_id);
        Ok(())
    }
}

impl ZoneAuthority {
    fn records(&self) -> MutexGuard<'_, Vec<(RecordId, Arc<str>)>> {
        recover_poisoned(self.records.lock())
    }

    /// The zone that owns the challenge name `fqdn`, if it was prepared.
    fn zone_of_name(&self, fqdn: &str) -> Option<Arc<str>> {
        let fqdn = without_root(fqdn);
        self.zones
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(fqdn))
            .map(|(_, zone)| Arc::clone(zone))
    }

    /// The zone of a record this authority created, if it still exists.
    fn zone_of_record(&self, record_id: &str) -> Option<Arc<str>> {
        self.records()
            .iter()
            .find(|(id, _)| &**id == record_id)
            .map(|(_, zone)| Arc::clone(zone))
    }

    fn remember(&self, record_id: RecordId, zone: Arc<str>) {
        self.records().push((record_id, zone));
    }

    fn forget(&self, record_id: &str) {
        self.records().retain(|(id, _)| &**id != record_id);
    }
}

/// The name whose zone serves the canonical `domain`: a wildcard's own base.
fn zone_subject(domain: &str) -> &str {
    domain.strip_prefix(WILDCARD_PREFIX).unwrap_or(domain)
}

/// A bearer token is nonempty visible ASCII.
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic())
}

/// A base is an absolute `https` URL, or `http` to a loopback host, naming
/// where requests go and never who sends them.
fn valid_base(base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    let transport = match url.scheme() {
        "https" => !host.is_empty(),
        "http" => is_loopback(host),
        _ => false,
    };
    transport && anonymous(&url) && url.query().is_none()
}

fn is_loopback(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || bare.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[derive(Serialize)]
struct CreateRecord<'a> {
    r#type: &'a str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
}

#[derive(Deserialize)]
struct CfZone {
    id: Box<str>,
    name: Box<str>,
}

impl CfZone {
    /// Whether this zone is exactly `candidate`, with an ID to address it by.
    fn names(&self, candidate: &str) -> bool {
        !self.id.is_empty() && without_root(&self.name).eq_ignore_ascii_case(candidate)
    }
}

#[derive(Deserialize)]
struct CfRecord {
    id: Box<str>,
}

impl CfRecord {
    /// The record's ID, when the answer named one.
    fn acknowledged(self, operation: IntegrationOperation) -> Result<RecordId, IntegrationError> {
        match self.id.is_empty() {
            true => Err(outcome_unknown(operation)),
            false => Ok(self.id),
        }
    }
}
