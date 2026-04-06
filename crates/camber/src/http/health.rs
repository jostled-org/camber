use crate::RuntimeError;
use crate::resource::{MIN_HEALTH_INTERVAL, Resource};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe a URL and return `true` if the response is a success status.
async fn probe_url(client: &reqwest::Client, url: &str) -> bool {
    client
        .get(url)
        .timeout(HEALTH_CHECK_TIMEOUT)
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

impl std::fmt::Debug for ProxyHealthResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyHealthResource")
            .field("name", &self.name)
            .field("url", &self.url)
            .finish()
    }
}

/// A proxy backend health check that implements [`Resource`] for lifecycle
/// integration.
///
/// Create with [`ProxyHealthResource::new`], pass the [`routing_flag`] to
/// [`Router::proxy_checked`], and register the resource with
/// [`RuntimeBuilder::resource`]. The runtime manages the health check interval.
///
/// [`routing_flag`]: ProxyHealthResource::routing_flag
/// [`Router::proxy_checked`]: super::Router::proxy_checked
/// [`RuntimeBuilder::resource`]: crate::RuntimeBuilder::resource
pub struct ProxyHealthResource {
    name: Box<str>,
    url: Box<str>,
    routing_flag: Arc<AtomicBool>,
}

impl ProxyHealthResource {
    /// Create a new proxy health resource.
    ///
    /// `backend` is the base URL (e.g., `"http://localhost:8080"`).
    /// `path` is the health endpoint path (e.g., `"/health"`).
    ///
    /// The routing flag starts `true` (healthy).
    pub fn new(backend: &str, path: &str) -> Self {
        Self {
            name: Box::from(backend),
            url: format!("{backend}{path}").into_boxed_str(),
            routing_flag: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Get the routing flag for use with [`Router::proxy_checked`].
    ///
    /// The runtime updates this flag based on health check results.
    /// The proxy router reads it to decide whether to forward requests.
    ///
    /// [`Router::proxy_checked`]: super::Router::proxy_checked
    pub fn routing_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.routing_flag)
    }
}

impl Resource for ProxyHealthResource {
    fn name(&self) -> &str {
        &self.name
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        let client = super::async_proxy::proxy_client()?;
        let handle = tokio::runtime::Handle::current();
        let url: &str = &self.url;
        let ok = handle.block_on(probe_url(client, url));
        self.routing_flag.store(ok, Ordering::Release);
        match ok {
            true => Ok(()),
            false => Err(RuntimeError::Http("backend health check failed".into())),
        }
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// Spawn a background Tokio task that polls a backend health endpoint.
///
/// Performs an initial probe before returning, so the flag reflects the
/// backend's real state immediately. The background loop then continues
/// polling at `interval`.
///
/// Returns an `Arc<AtomicBool>` that reflects the backend's health state.
/// On poll failure it flips to `false`; on success it flips back to `true`.
///
/// For lifecycle integration (health reporting via `/health`, structured
/// shutdown), use [`ProxyHealthResource`] with [`RuntimeBuilder::resource`]
/// instead.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is less than 1 second.
///
/// [`RuntimeBuilder::resource`]: crate::RuntimeBuilder::resource
pub async fn spawn_health_checker(
    backend: &str,
    path: &str,
    interval: Duration,
) -> Result<Arc<AtomicBool>, RuntimeError> {
    if interval < MIN_HEALTH_INTERVAL {
        return Err(RuntimeError::InvalidArgument(
            "health check interval must be at least 1 second".into(),
        ));
    }
    let url = format!("{backend}{path}");

    // Reuse the shared no-proxy client from async_proxy to avoid a second
    // connection pool and TLS session cache.
    let client = super::async_proxy::proxy_client()?.clone();

    // Initial probe before spawning the background loop.
    let initial_ok = probe_url(&client, &url).await;
    let healthy = Arc::new(AtomicBool::new(initial_ok));
    let flag = Arc::clone(&healthy);

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let ok = probe_url(&client, &url).await;
            flag.store(ok, Ordering::Release);
        }
    });

    Ok(healthy)
}
