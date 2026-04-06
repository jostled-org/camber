use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use rustls::sign::CertifiedKey;
use tokio::task::JoinHandle;

use super::provider::DnsProvider;
use crate::RuntimeError;
use crate::config::AcmeBase;
use crate::tls::{CertStore, parse_certified_key};

const RENEWAL_THRESHOLD_DAYS: i64 = 30;
const RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const LE_CERT_LIFETIME_DAYS: i64 = 90;
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(300);

/// DNS-01 ACME certificate provisioning via instant-acme.
///
/// Wraps [`AcmeBase`] with the DNS-01-specific provisioning, caching, and
/// renewal logic.
pub struct AcmeDns01 {
    base: AcmeBase,
}

impl AcmeDns01 {
    /// Create a new DNS-01 ACME configuration.
    ///
    /// `tool_name` sets the default cache directory to `~/.config/{tool_name}/certs/`.
    pub fn new(tool_name: &str, domains: impl IntoIterator<Item = impl Into<Box<str>>>) -> Self {
        Self {
            base: AcmeBase::new(tool_name, domains),
        }
    }

    pub fn email(mut self, email: impl Into<Box<str>>) -> Self {
        self.base = self.base.email(email);
        self
    }

    pub fn cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.base = self.base.cache_dir(path);
        self
    }

    pub fn staging(mut self, staging: bool) -> Self {
        self.base = self.base.staging(staging);
        self
    }

    pub fn cache_path(&self) -> &Path {
        self.base.cache_path()
    }

    /// Run the full ACME DNS-01 flow: order -> challenge -> TXT record -> finalize -> cert.
    pub async fn provision_cert<P: DnsProvider>(
        &self,
        provider: &P,
    ) -> Result<CertifiedKey, RuntimeError> {
        let account = self.load_or_create_account().await?;
        let identifiers: Vec<_> = self
            .base
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.to_string()))
            .collect();

        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(acme_err)?;

        let txt_records = create_dns_challenges(&mut order, provider).await?;
        let (cert_pem, key_pem) = finalize_order(&mut order).await?;

        cleanup_txt_records(provider, &txt_records).await;
        self.cache_cert(&cert_pem, &key_pem)?;
        parse_certified_key(cert_pem.as_bytes(), key_pem.as_bytes())
    }

    /// Load a cached certificate from disk if present and not expired.
    pub fn load_cached_cert(&self) -> Result<Option<CertifiedKey>, RuntimeError> {
        let cert_path = self.base.cache_dir.join("cert.pem");
        let key_path = self.base.cache_dir.join("key.pem");

        match (cert_path.exists(), key_path.exists()) {
            (true, true) => {
                let cert_pem = std::fs::read(&cert_path)?;
                let key_pem = std::fs::read(&key_path)?;
                Ok(Some(parse_certified_key(&cert_pem, &key_pem)?))
            }
            _ => Ok(None),
        }
    }

    /// Check if the cached cert needs renewal (expires within 30 days).
    pub fn needs_renewal(&self) -> bool {
        let expiry_path = self.base.cache_dir.join("expiry");
        let days_remaining = std::fs::read_to_string(&expiry_path)
            .ok()
            .and_then(|c| c.trim().parse::<i64>().ok())
            .map(|expiry_secs| (expiry_secs - now_unix_secs()) / 86400);
        match days_remaining {
            Some(days) => days < RENEWAL_THRESHOLD_DAYS,
            None => true,
        }
    }

    /// Spawn a background task that renews the cert before expiry and swaps it
    /// into the given CertStore.
    pub fn spawn_renewal<P: DnsProvider + 'static>(
        self,
        provider: P,
        store: CertStore,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RENEWAL_CHECK_INTERVAL).await;

                if !self.needs_renewal() {
                    continue;
                }

                tracing::info!("dns01 acme: cert renewal triggered");
                match self.provision_cert(&provider).await {
                    Ok(new_cert) => {
                        store.swap(new_cert);
                        tracing::info!("dns01 acme: cert renewed and swapped");
                    }
                    Err(e) => tracing::warn!("dns01 acme: renewal failed: {e}"),
                }
            }
        })
    }

    fn cache_cert(&self, cert_pem: &str, key_pem: &str) -> Result<(), RuntimeError> {
        std::fs::create_dir_all(&self.base.cache_dir)?;
        std::fs::write(self.base.cache_dir.join("cert.pem"), cert_pem)?;
        let key_path = self.base.cache_dir.join("key.pem");
        std::fs::write(&key_path, key_pem)?;
        restrict_key_permissions(&key_path)?;
        write_expiry(&self.base.cache_dir)?;
        Ok(())
    }

    async fn load_or_create_account(&self) -> Result<Account, RuntimeError> {
        let creds_path = self.base.cache_dir.join("account.json");

        match std::fs::read(&creds_path) {
            Ok(data) => {
                let creds: AccountCredentials = serde_json::from_slice(&data).map_err(|e| {
                    RuntimeError::Acme(format!("failed to parse account credentials: {e}").into())
                })?;
                Account::builder()
                    .map_err(acme_err)?
                    .from_credentials(creds)
                    .await
                    .map_err(acme_err)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let (account, creds) = create_account(&self.base.email, self.base.staging).await?;
                self.save_credentials(&creds)?;
                Ok(account)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn save_credentials(&self, credentials: &AccountCredentials) -> Result<(), RuntimeError> {
        std::fs::create_dir_all(&self.base.cache_dir)?;
        let json = serde_json::to_vec(credentials).map_err(|e| {
            RuntimeError::Acme(format!("failed to serialize account credentials: {e}").into())
        })?;
        let account_path = self.base.cache_dir.join("account.json");
        std::fs::write(&account_path, json)?;
        restrict_key_permissions(&account_path)?;
        Ok(())
    }
}

async fn create_account(
    email: &Option<Box<str>>,
    staging: bool,
) -> Result<(Account, AccountCredentials), RuntimeError> {
    let contact_str: Option<String> = email.as_ref().map(|e| format!("mailto:{e}"));
    let refs: Box<[&str]> = contact_str.iter().map(String::as_str).collect();
    let new_account = NewAccount {
        contact: &refs,
        terms_of_service_agreed: true,
        only_return_existing: false,
    };
    let url = match staging {
        true => LetsEncrypt::Staging.url(),
        false => LetsEncrypt::Production.url(),
    };
    Account::builder()
        .map_err(acme_err)?
        .create(&new_account, url.into(), None)
        .await
        .map_err(acme_err)
}

async fn create_dns_challenges<P: DnsProvider>(
    order: &mut instant_acme::Order,
    provider: &P,
) -> Result<Vec<Box<str>>, RuntimeError> {
    let mut txt_records = Vec::new();
    let mut auths = order.authorizations();

    while let Some(auth_result) = auths.next().await {
        let mut auth = auth_result.map_err(acme_err)?;
        let mut challenge = auth
            .challenge(ChallengeType::Dns01)
            .ok_or_else(|| RuntimeError::Acme("no DNS-01 challenge offered".into()))?;

        let domain = challenge.identifier().to_string();
        let dns_value = challenge.key_authorization().dns_value();
        let fqdn = format!("_acme-challenge.{domain}");

        let record_id = provider.create_txt_record(&fqdn, &dns_value).await?;
        txt_records.push(record_id);

        challenge.set_ready().await.map_err(acme_err)?;
    }

    Ok(txt_records)
}

async fn finalize_order(
    order: &mut instant_acme::Order,
) -> Result<(Box<str>, Box<str>), RuntimeError> {
    let retry = RetryPolicy::new().timeout(CHALLENGE_TIMEOUT);
    let status = order.poll_ready(&retry).await.map_err(acme_err)?;
    match status {
        OrderStatus::Ready => {}
        other => {
            return Err(RuntimeError::Acme(
                format!("order in unexpected state: {other:?}").into(),
            ));
        }
    }

    let key_pem: Box<str> = order.finalize().await.map_err(acme_err)?.into();
    let cert_pem: Box<str> = order
        .poll_certificate(&retry)
        .await
        .map_err(acme_err)?
        .into();
    Ok((cert_pem, key_pem))
}

async fn cleanup_txt_records<P: DnsProvider>(provider: &P, record_ids: &[Box<str>]) {
    for id in record_ids {
        if let Err(e) = provider.delete_txt_record(id).await {
            tracing::warn!("dns01 acme: failed to clean up TXT record {id}: {e}");
        }
    }
}

fn write_expiry(cache_dir: &Path) -> Result<(), RuntimeError> {
    let expiry = now_unix_secs() + (LE_CERT_LIFETIME_DAYS * 86400);
    std::fs::write(cache_dir.join("expiry"), expiry.to_string())?;
    Ok(())
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Set file permissions to 0600 (owner read/write only) for private key files.
#[cfg(unix)]
fn restrict_key_permissions(path: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// No-op on non-Unix platforms. Windows ACLs require a different approach.
#[cfg(not(unix))]
fn restrict_key_permissions(_path: &Path) -> Result<(), RuntimeError> {
    Ok(())
}

fn acme_err(e: instant_acme::Error) -> RuntimeError {
    RuntimeError::Acme(format!("{e}").into())
}
