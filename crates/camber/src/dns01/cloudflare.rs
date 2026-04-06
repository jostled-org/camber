use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::provider::{DnsProvider, RecordId};
use crate::RuntimeError;

const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

/// Cloudflare DNS provider for ACME DNS-01 challenges.
pub struct CloudflareProvider {
    client: Client,
    api_token: Box<str>,
    zone_id: Box<str>,
    base_url: Box<str>,
}

impl CloudflareProvider {
    /// Create a provider for the given domain, looking up the zone ID via API.
    pub async fn new(api_token: Box<str>, domain: &str) -> Result<Self, RuntimeError> {
        Self::with_base_url(api_token, domain, CLOUDFLARE_API.into()).await
    }

    /// Create a provider with a custom base URL (for testing).
    pub async fn with_base_url(
        api_token: Box<str>,
        domain: &str,
        base_url: Box<str>,
    ) -> Result<Self, RuntimeError> {
        let client = Client::new();
        let zone_id = lookup_zone_id(&client, &api_token, domain, &base_url).await?;
        Ok(Self {
            client,
            api_token,
            zone_id,
            base_url,
        })
    }
}

impl DnsProvider for CloudflareProvider {
    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<RecordId, RuntimeError> {
        let url = format!("{}/zones/{}/dns_records", self.base_url, self.zone_id);
        let body = CreateRecord {
            r#type: "TXT",
            name: fqdn,
            content: value,
            ttl: 120,
        };

        let resp = send_request(
            self.client
                .post(&url)
                .bearer_auth(&*self.api_token)
                .json(&body),
        )
        .await?;
        let record: CfRecord = parse_cf_body(resp).await?;
        Ok(record.id)
    }

    async fn delete_txt_record(&self, record_id: &str) -> Result<(), RuntimeError> {
        let url = format!(
            "{}/zones/{}/dns_records/{record_id}",
            self.base_url, self.zone_id,
        );

        let resp = send_request(self.client.delete(&url).bearer_auth(&*self.api_token)).await?;
        check_cf_success(resp).await
    }
}

async fn send_request(request: reqwest::RequestBuilder) -> Result<reqwest::Response, RuntimeError> {
    request
        .send()
        .await
        .map_err(|e| RuntimeError::Dns(format!("request failed: {e}").into()))
}

/// Walk up the domain hierarchy to find the matching Cloudflare zone.
///
/// For `app.bar.example.com`, tries: `app.bar.example.com`, `bar.example.com`,
/// `example.com`. Stops before querying bare TLDs.
async fn lookup_zone_id(
    client: &Client,
    api_token: &str,
    domain: &str,
    base_url: &str,
) -> Result<Box<str>, RuntimeError> {
    let url = format!("{base_url}/zones");
    let mut candidate = domain;

    while candidate.contains('.') {
        let resp = send_request(
            client
                .get(&url)
                .bearer_auth(api_token)
                .query(&[("name", candidate)]),
        )
        .await?;
        let zones: Vec<CfZone> = parse_cf_body(resp).await?;

        if let Some(zone) = zones.into_iter().next() {
            return Ok(zone.id);
        }

        // Safe: while guard ensures '.' exists
        candidate = &candidate[candidate.find('.').unwrap_or(0) + 1..];
    }

    Err(RuntimeError::Dns(
        format!("no zone found for domain {domain}").into(),
    ))
}

/// Parse a Cloudflare API response, extracting the result on success
/// or returning a descriptive error on failure.
async fn parse_cf_body<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
) -> Result<T, RuntimeError> {
    let status = resp.status();
    let body: CfResponse<T> = resp.json().await.map_err(|e| {
        RuntimeError::Dns(format!("failed to parse response (HTTP {status}): {e}").into())
    })?;

    match body.success {
        true => body
            .result
            .ok_or_else(|| RuntimeError::Dns("cloudflare returned success with no result".into())),
        false => Err(cf_error(&body.errors)),
    }
}

/// Check a Cloudflare response for success without deserializing the result.
async fn check_cf_success(resp: reqwest::Response) -> Result<(), RuntimeError> {
    let status = resp.status();
    let body: CfStatus = resp.json().await.map_err(|e| {
        RuntimeError::Dns(format!("failed to parse response (HTTP {status}): {e}").into())
    })?;
    match body.success {
        true => Ok(()),
        false => Err(cf_error(&body.errors)),
    }
}

fn cf_error(errors: &[CfError]) -> RuntimeError {
    let msg = match errors.first() {
        Some(e) => &*e.message,
        None => "unknown cloudflare error",
    };
    RuntimeError::Dns(format!("cloudflare API error: {msg}").into())
}

#[derive(Serialize)]
struct CreateRecord<'a> {
    r#type: &'a str,
    name: &'a str,
    content: &'a str,
    ttl: u32,
}

#[derive(Deserialize)]
struct CfResponse<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<CfError>,
}

#[derive(Deserialize)]
struct CfError {
    message: Box<str>,
}

#[derive(Deserialize)]
struct CfStatus {
    success: bool,
    #[serde(default)]
    errors: Vec<CfError>,
}

#[derive(Deserialize)]
struct CfZone {
    id: Box<str>,
}

#[derive(Deserialize)]
struct CfRecord {
    id: Box<str>,
}
