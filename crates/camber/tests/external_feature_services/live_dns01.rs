#![cfg(feature = "dns01")]

use crate::resources::{CleanupWitness, ExternalRun};
use camber::dns01::AcmeDns01;
use reqwest::{Client, Response};
use serde::Deserialize;
use std::sync::Arc;
use tempfile::TempDir;

const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

struct CloudflareCleanupProbe {
    client: Client,
    token: Arc<str>,
    zone_id: Box<str>,
}

impl CloudflareCleanupProbe {
    async fn new(token: Arc<str>, domain: &str) -> Result<Self, Box<str>> {
        let client = Client::new();
        let zone_id = lookup_zone_id(&client, &token, domain).await?;
        Ok(Self {
            client,
            token,
            zone_id,
        })
    }

    async fn txt_records(&self, fqdn: &str) -> Result<Box<[CloudflareRecord]>, Box<str>> {
        let response = self
            .client
            .get(format!(
                "{CLOUDFLARE_API}/zones/{}/dns_records",
                self.zone_id
            ))
            .bearer_auth(&*self.token)
            .query(&[("type", "TXT"), ("name", fqdn), ("per_page", "100")])
            .send()
            .await
            .map_err(api_request_error)?;
        let records: Vec<CloudflareRecord> = cloudflare_result(response).await?;
        Ok(records.into_boxed_slice())
    }

    async fn delete_records(&self, records: &[CloudflareRecord]) -> Result<(), Box<str>> {
        for record in records {
            let response = self
                .client
                .delete(format!(
                    "{CLOUDFLARE_API}/zones/{}/dns_records/{}",
                    self.zone_id, record.id
                ))
                .bearer_auth(&*self.token)
                .send()
                .await
                .map_err(api_request_error)?;
            cloudflare_success(response).await?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct CloudflareEnvelope<T> {
    success: bool,
    result: Option<T>,
    #[serde(default)]
    errors: Vec<CloudflareError>,
}

#[derive(Deserialize)]
struct CloudflareError {
    message: Box<str>,
}

#[derive(Deserialize)]
struct CloudflareZone {
    id: Box<str>,
}

#[derive(Deserialize)]
struct CloudflareRecord {
    id: Box<str>,
}

async fn lookup_zone_id(client: &Client, token: &str, domain: &str) -> Result<Box<str>, Box<str>> {
    let mut candidate = domain;

    while candidate.contains('.') {
        let response = client
            .get(format!("{CLOUDFLARE_API}/zones"))
            .bearer_auth(token)
            .query(&[("name", candidate)])
            .send()
            .await
            .map_err(api_request_error)?;
        let zones: Vec<CloudflareZone> = cloudflare_result(response).await?;
        if let Some(zone) = zones.into_iter().next() {
            return Ok(zone.id);
        }
        candidate = match candidate.split_once('.') {
            Some((_, parent)) => parent,
            None => break,
        };
    }

    Err(format!("Cloudflare zone not found for {domain}").into_boxed_str())
}

async fn cloudflare_result<T: serde::de::DeserializeOwned>(
    response: Response,
) -> Result<T, Box<str>> {
    let status = response.status();
    let body: CloudflareEnvelope<T> = response
        .json()
        .await
        .map_err(|error| format!("Cloudflare HTTP {status} response: {error}").into_boxed_str())?;
    match (status.is_success(), body.success, body.result) {
        (true, true, Some(result)) => Ok(result),
        _ => Err(cloudflare_error(status, &body.errors)),
    }
}

async fn cloudflare_success(response: Response) -> Result<(), Box<str>> {
    let status = response.status();
    let body: CloudflareEnvelope<serde_json::Value> = response
        .json()
        .await
        .map_err(|error| format!("Cloudflare HTTP {status} response: {error}").into_boxed_str())?;
    match (status.is_success(), body.success) {
        (true, true) => Ok(()),
        _ => Err(cloudflare_error(status, &body.errors)),
    }
}

fn cloudflare_error(status: reqwest::StatusCode, errors: &[CloudflareError]) -> Box<str> {
    let detail = errors
        .first()
        .map_or("unknown Cloudflare API error", |error| &error.message);
    format!("Cloudflare HTTP {status}: {detail}").into_boxed_str()
}

fn api_request_error(error: reqwest::Error) -> Box<str> {
    format!("Cloudflare request failed: {error}").into_boxed_str()
}

#[tokio::test]
#[ignore = "external lane dns; owner: Camber ACME and DNS integrations; run: gh workflow run external-evidence.yml -f lane=dns"]
async fn acme_dns01_provisions_cert() {
    let run = ExternalRun::from_environment().expect("valid CAMBER_EXTERNAL_RUN_ID");
    let witness =
        CleanupWitness::from_environment().expect("valid CAMBER_EXTERNAL_CLEANUP_WITNESS path");
    let base_domain = std::env::var("ACME_TEST_DOMAIN").expect("ACME_TEST_DOMAIN must be set");
    let domain = run
        .dns_subdomain(&base_domain)
        .expect("unique ACME test subdomain");
    let challenge_fqdn = format!("_acme-challenge.{domain}").into_boxed_str();
    let token: Arc<str> = std::env::var("CF_TOKEN")
        .expect("CF_TOKEN must be set")
        .into();

    let provider = camber::dns01::CloudflareProvider::new((&*token).into(), &domain)
        .await
        .expect("cloudflare provider");
    let probe = CloudflareCleanupProbe::new(token, &domain)
        .await
        .expect("Cloudflare cleanup probe");
    assert!(
        probe
            .txt_records(&challenge_fqdn)
            .await
            .expect("query initial TXT records")
            .is_empty(),
        "unique challenge name must start without TXT records"
    );

    let cache_dir = TempDir::new().expect("temp dir");
    let config = AcmeDns01::new("test", [&*domain])
        .email("test@example.com")
        .cache_dir(cache_dir.path())
        .staging(true);

    let provision_result = config.provision_cert(&provider).await;
    let residual_records = probe
        .txt_records(&challenge_fqdn)
        .await
        .expect("query TXT records after provisioning");
    let provider_cleanup_completed = residual_records.is_empty();

    // A failed ACME finalization can bypass the library cleanup path. Remove any
    // run-scoped residue before reporting the original result.
    probe
        .delete_records(&residual_records)
        .await
        .expect("remove residual TXT records");
    let cleanup_visible = probe
        .txt_records(&challenge_fqdn)
        .await
        .expect("verify TXT cleanup through Cloudflare")
        .is_empty();
    assert!(
        cleanup_visible,
        "Cloudflare still exposes challenge TXT records"
    );

    let cert_cached = cache_dir.path().join("cert.pem").exists();
    let key_cached = cache_dir.path().join("key.pem").exists();
    let (cert_chain_present, provision_error) = match provision_result {
        Ok(cert) => {
            let present = !cert.cert.is_empty();
            drop(cert);
            (present, None)
        }
        Err(error) => (false, Some(error)),
    };

    drop(config);
    drop(provider);
    drop(probe);
    cache_dir.close().expect("remove certificate cache");

    witness
        .emit(&run, &[&domain, &challenge_fqdn])
        .expect("emit cleanup witness after DNS resources drop");

    assert!(
        provider_cleanup_completed,
        "AcmeDns01 left challenge TXT records for fallback cleanup"
    );
    assert!(
        provision_error.is_none(),
        "certificate provisioning failed after cleanup: {provision_error:?}"
    );
    assert!(cert_chain_present, "cert chain should not be empty");
    assert!(cert_cached, "cert cached to disk");
    assert!(key_cached, "key cached to disk");
}
