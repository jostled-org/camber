use camber::config::{AcmeSettings, TlsMode};
use camber_cli::config::Config;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::CliError;

pub fn run(config_path: &Path) -> Result<(), CliError> {
    let config = Config::load(config_path)?;

    let mut builder = camber::runtime::builder();

    if let Some(limit) = config.connection_limit() {
        builder = builder.connection_limit(limit);
    }

    if let Some(tls) = config.tls() {
        builder = apply_tls(builder, tls, &config)?;
    }

    builder.run(|| serve_from_config(&config))?
}

fn apply_tls(
    builder: camber::RuntimeBuilder,
    tls: &TlsMode,
    config: &Config,
) -> Result<camber::RuntimeBuilder, CliError> {
    match tls {
        TlsMode::Manual { cert, key } => Ok(builder
            .tls_cert(Path::new(&**cert))
            .tls_key(Path::new(&**key))),
        TlsMode::TlsAlpn(acme) => Ok(builder.tls_auto(apply_acme_settings(
            camber::acme::AcmeConfig::new("camber", config.auto_tls_domains()),
            acme,
            |cfg, e| cfg.email(e),
            |cfg, s| cfg.staging(s),
            |cfg, d| cfg.cache_dir(d),
        ))),
        TlsMode::Dns01 { acme, token } => {
            // Every site's name is certified, and each prepares its own zone.
            // `Config::load` already validated these names, so a refused
            // configuration never reaches the secret source.
            let acme = apply_acme_settings(
                camber::dns01::AcmeDns01::new("camber", config.auto_tls_domains()),
                acme,
                |cfg, e| cfg.email(e),
                |cfg, s| cfg.staging(s),
                |cfg, d| cfg.cache_dir(d),
            );
            let token = camber::secret::load_secret(token)
                .map_err(|e| CliError::Config(e.to_string().into()))?;
            Ok(builder.tls_auto_dns01(acme, token))
        }
    }
}

/// Apply the shared ACME settings to either challenge's configuration.
fn apply_acme_settings<T>(
    base: T,
    acme: &AcmeSettings,
    set_email: fn(T, &str) -> T,
    set_staging: fn(T, bool) -> T,
    set_cache_dir: fn(T, &str) -> T,
) -> T {
    let configured = set_staging(set_email(base, &acme.email), acme.staging);
    match acme.cache_dir.as_deref() {
        Some(dir) => set_cache_dir(configured, dir),
        None => configured,
    }
}

/// One overlay site's shared state: its local root, its upstream, and the
/// health authority its streaming proxy routes also read.
struct OverlaySite {
    base_dir: Arc<Path>,
    backend: Arc<str>,
    health: Option<Arc<AtomicBool>>,
}

/// What an overlay fallback answers while the site's upstream is unhealthy.
///
/// The same status and body the routed proxy refusal answers. The routed
/// refusal also carries `X-Request-Id`; this one does not, because a handler
/// cannot raise that refusal itself.
const UNHEALTHY_BODY: &str = "service unavailable";

/// Serve one overlay request: local file first, then fall back to proxy.
fn overlay(
    site: &Arc<OverlaySite>,
    req: &camber::http::Request,
) -> impl std::future::Future<Output = camber::http::HandlerOutcome> + Send + use<> {
    let raw_path = req.param("proxy_path").unwrap_or("");
    let file_path: Box<str> = match raw_path.is_empty() {
        true => "index.html".into(),
        false => raw_path.into(),
    };
    let site = Arc::clone(site);
    let proxy_fut = camber::http::proxy_forward(req, &site.backend, "");
    // No `spawn_blocking` here: the static-file entry point offloads its own
    // filesystem work, so wrapping it would only put one blocking thread in
    // front of another.
    async move {
        match camber::http::serve_file(&site.base_dir, &file_path).await {
            Ok(file_resp) if file_resp.status() != 404 => Ok(file_resp),
            Ok(_) => overlay_fallback(&site, proxy_fut).await,
            // The overlay still falls back, but a refused file is a
            // configuration answer — a crossed `ByteBoundary::StaticFile`
            // or an unreadable root — and it reaches the operator rather
            // than vanishing behind an upstream response.
            Err(error) => {
                camber::tracing::warn!(
                    %file_path,
                    %error,
                    "overlay file refused; falling back to the proxy"
                );
                overlay_fallback(&site, proxy_fut).await
            }
        }
    }
}

/// Forward a local miss, or refuse it while the site's upstream is unhealthy.
///
/// The health authority is read here, at the fallback, so a local hit never
/// depends on the upstream's state.
async fn overlay_fallback(
    site: &OverlaySite,
    proxy_fut: impl std::future::Future<Output = camber::http::Response>,
) -> camber::http::HandlerOutcome {
    match upstream_unhealthy(site.health.as_deref()) {
        true => camber::http::Response::text(503, UNHEALTHY_BODY),
        false => Ok(proxy_fut.await),
    }
}

/// Whether a site's health authority holds its upstream unhealthy.
///
/// `None` is a site with no health check, which is never unhealthy. The load
/// matches the routed proxy's own read of the same flag.
fn upstream_unhealthy(health: Option<&AtomicBool>) -> bool {
    health.is_some_and(|flag| !flag.load(Ordering::Relaxed))
}

fn serve_from_config(config: &Config) -> Result<(), CliError> {
    let mut host_router = camber::http::HostRouter::new();

    for site in config.sites() {
        let mut router = camber::http::Router::new();

        match (site.proxy(), site.root()) {
            (Some(backend), Some(root)) => {
                let health = spawn_site_health(site, backend)?;
                register_overlay_site(&mut router, backend, root, health);
            }
            (Some(backend), None) => {
                let health = spawn_site_health(site, backend)?;
                register_streaming_proxy(&mut router, backend, health);
            }
            (None, Some(root)) => {
                router.static_files("", root);
            }
            (None, None) => {}
        }

        host_router.add(site.routing_host(), router);
    }

    let listener = camber::net::listen(config.listen())?;
    camber::tracing::info!("listening on {}", config.listen());

    camber::http::serve_hosts(listener, host_router)?;
    Ok(())
}

/// Start the site's health authority, if it declares a health check.
///
/// The returned flag is the site's one authority: every route that reaches
/// the upstream reads it, so no path forwards a request another path refuses.
fn spawn_site_health(
    site: &camber_cli::config::SiteConfig,
    backend: &str,
) -> Result<Option<Arc<AtomicBool>>, CliError> {
    match site.health_check() {
        Some(path) => {
            let interval = std::time::Duration::from_secs(site.health_interval().unwrap_or(10));
            let healthy = camber::runtime::block_on(camber::http::spawn_health_checker(
                backend, path, interval,
            ))?;
            Ok(Some(healthy))
        }
        None => Ok(None),
    }
}

/// Register streaming proxy routes under the site's health authority, if any.
fn register_streaming_proxy(
    router: &mut camber::http::Router,
    backend: &str,
    health: Option<Arc<AtomicBool>>,
) {
    match health {
        Some(healthy) => router.proxy_checked_stream("", backend, healthy),
        None => router.proxy_stream("", backend),
    }
}

/// Register a site with both proxy and root using the local-file overlay.
///
/// GET/HEAD requests try the local file first; if the file does not exist,
/// the request falls back to the proxy backend. Non-GET/HEAD requests
/// always go to the backend via the streaming proxy path. Both paths read
/// the same health authority.
fn register_overlay_site(
    router: &mut camber::http::Router,
    backend: &str,
    root: &str,
    health: Option<Arc<AtomicBool>>,
) {
    // Register streaming proxy for all methods first.
    // The GET/HEAD handlers will be overridden below.
    register_streaming_proxy(router, backend, health.as_ref().map(Arc::clone));

    let site = Arc::new(OverlaySite {
        base_dir: Arc::from(Path::new(root)),
        backend: backend.into(),
        health,
    });

    // Override GET and HEAD with the overlay handler: local file first, proxy fallback.
    // The wildcard name must match the proxy_stream registration (proxy_path).
    // insert_proxy_routes registers "/*proxy_path" and "/", so both are
    // overridden to ensure "/" serves index.html from the local root.
    let handler = move |req: &camber::http::Request| overlay(&site, req);
    router.get("/*proxy_path", handler.clone());
    router.get("/", handler.clone());
    router.head("/*proxy_path", handler.clone());
    router.head("/", handler);
}
