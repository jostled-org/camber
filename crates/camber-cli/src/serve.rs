use camber_cli::config::Config;
use std::path::Path;

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
    tls: &camber_cli::config::TlsConfig,
    config: &Config,
) -> Result<camber::RuntimeBuilder, CliError> {
    match (
        tls.auto(),
        tls.email(),
        tls.dns_provider(),
        tls.cert(),
        tls.key(),
    ) {
        (true, Some(email), Some(_), _, _) => apply_dns01_tls(builder, tls, config, email),
        (true, Some(email), None, _, _) => apply_http01_tls(builder, tls, config, email),
        (true, None, _, _, _) => Err(CliError::Config("tls: auto = true requires email".into())),
        (false, _, _, Some(cert), Some(key)) => {
            Ok(builder.tls_cert(Path::new(cert)).tls_key(Path::new(key)))
        }
        _ => Err(CliError::Config(
            "tls: both cert and key must be provided".into(),
        )),
    }
}

fn apply_http01_tls(
    builder: camber::RuntimeBuilder,
    tls: &camber_cli::config::TlsConfig,
    config: &Config,
    email: &str,
) -> Result<camber::RuntimeBuilder, CliError> {
    let domains = config.auto_tls_domains();
    let acme_config = build_acme_http01(domains, email, tls.staging(), tls.cache_dir());
    Ok(builder.tls_auto(acme_config))
}

fn build_acme_http01(
    domains: impl IntoIterator<Item = impl Into<Box<str>>>,
    email: &str,
    staging: bool,
    cache_dir: Option<&str>,
) -> camber::acme::AcmeConfig {
    apply_acme_base(
        camber::acme::AcmeConfig::new("camber", domains),
        email,
        staging,
        cache_dir,
        |cfg, e| cfg.email(e),
        |cfg, s| cfg.staging(s),
        |cfg, d| cfg.cache_dir(d),
    )
}

fn apply_dns01_tls(
    builder: camber::RuntimeBuilder,
    tls: &camber_cli::config::TlsConfig,
    config: &Config,
    email: &str,
) -> Result<camber::RuntimeBuilder, CliError> {
    let domains = config.auto_tls_domains();
    let first_host: Box<str> = (*domains
        .first()
        .ok_or_else(|| CliError::Config("tls: no site hosts for DNS-01".into()))?)
    .into();

    let token = load_dns_token(tls)?;

    let acme = build_acme_dns01(domains, email, tls.staging(), tls.cache_dir());

    Ok(builder.tls_auto_dns01(acme, token, first_host))
}

fn build_acme_dns01(
    domains: impl IntoIterator<Item = impl Into<Box<str>>>,
    email: &str,
    staging: bool,
    cache_dir: Option<&str>,
) -> camber::dns01::AcmeDns01 {
    apply_acme_base(
        camber::dns01::AcmeDns01::new("camber", domains),
        email,
        staging,
        cache_dir,
        |cfg, e| cfg.email(e),
        |cfg, s| cfg.staging(s),
        |cfg, d| cfg.cache_dir(d),
    )
}

fn apply_acme_base<T>(
    base: T,
    email: &str,
    staging: bool,
    cache_dir: Option<&str>,
    set_email: fn(T, &str) -> T,
    set_staging: fn(T, bool) -> T,
    set_cache_dir: fn(T, &str) -> T,
) -> T {
    let configured = set_staging(set_email(base, email), staging);
    match cache_dir {
        Some(dir) => set_cache_dir(configured, dir),
        None => configured,
    }
}

/// Build an overlay handler that serves local files first, then falls back to proxy.
fn overlay_handler(
    base_dir: std::sync::Arc<std::path::Path>,
    backend: std::sync::Arc<str>,
) -> impl Fn(
    &camber::http::Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = camber::http::Response> + Send>>
+ Send
+ Sync
+ 'static {
    move |req: &camber::http::Request| {
        let raw_path = req.param("proxy_path").unwrap_or("");
        let file_path: Box<str> = match raw_path.is_empty() {
            true => "index.html".into(),
            false => raw_path.into(),
        };
        let base = std::sync::Arc::clone(&base_dir);
        let backend = std::sync::Arc::clone(&backend);
        let proxy_fut = camber::http::proxy_forward(req, &backend, "");
        Box::pin(async move {
            let file_resp = camber::http::serve_file(&base, &file_path);
            match file_resp.status() {
                404 => proxy_fut.await,
                _ => file_resp,
            }
        })
    }
}

fn load_dns_token(tls: &camber_cli::config::TlsConfig) -> Result<Box<str>, CliError> {
    let secret_ref = match (tls.dns_api_token_env(), tls.dns_api_token_file()) {
        (Some(env), None) => camber::secret::SecretRef::Env(env.into()),
        (None, Some(file)) => camber::secret::SecretRef::File(file.into()),
        _ => {
            return Err(CliError::Config(
                "tls: dns_provider requires exactly one token source".into(),
            ));
        }
    };
    camber::secret::load_secret(&secret_ref).map_err(|e| CliError::Config(e.to_string().into()))
}

fn serve_from_config(config: &Config) -> Result<(), CliError> {
    let mut host_router = camber::http::HostRouter::new();

    for site in config.sites() {
        let mut router = camber::http::Router::new();

        match (site.proxy(), site.root()) {
            (Some(backend), Some(root)) => {
                register_overlay_site(&mut router, site, backend, root)?;
            }
            (Some(backend), None) => {
                register_proxy_site(&mut router, site, backend)?;
            }
            (None, Some(root)) => {
                router.static_files("", root);
            }
            (None, None) => {}
        }

        host_router.add(site.host(), router);
    }

    let listener = camber::net::listen(config.listen())?;
    camber::tracing::info!("listening on {}", config.listen());

    camber::http::serve_hosts(listener, host_router)?;
    Ok(())
}

/// Register streaming proxy routes with optional health checking.
fn register_streaming_proxy(
    router: &mut camber::http::Router,
    site: &camber_cli::config::SiteConfig,
    backend: &str,
) -> Result<(), CliError> {
    match site.health_check() {
        Some(path) => {
            let interval = std::time::Duration::from_secs(site.health_interval().unwrap_or(10));
            let healthy = camber::runtime::block_on(camber::http::spawn_health_checker(
                backend, path, interval,
            ))?;
            router.proxy_checked_stream("", backend, healthy);
        }
        None => {
            router.proxy_stream("", backend);
        }
    }
    Ok(())
}

/// Register a proxy-only site using streaming proxy.
fn register_proxy_site(
    router: &mut camber::http::Router,
    site: &camber_cli::config::SiteConfig,
    backend: &str,
) -> Result<(), CliError> {
    register_streaming_proxy(router, site, backend)
}

/// Register a site with both proxy and root using the local-file overlay.
///
/// GET/HEAD requests try the local file first; if the file does not exist,
/// the request falls back to the proxy backend. Non-GET/HEAD requests
/// always go to the backend via the streaming proxy path.
fn register_overlay_site(
    router: &mut camber::http::Router,
    site: &camber_cli::config::SiteConfig,
    backend: &str,
    root: &str,
) -> Result<(), CliError> {
    // Register streaming proxy for all methods first.
    // The GET/HEAD handlers will be overridden below.
    register_streaming_proxy(router, site, backend)?;

    // Override GET and HEAD with the overlay handler: local file first, proxy fallback.
    // The wildcard name must match the proxy_stream registration (proxy_path).
    let base_dir: std::sync::Arc<std::path::Path> =
        std::sync::Arc::from(std::path::PathBuf::from(root).into_boxed_path());
    let backend_arc: std::sync::Arc<str> = backend.into();

    // Override both wildcard and exact root for GET and HEAD.
    // insert_proxy_routes registers "/*proxy_path" and "/", so we must
    // override both to ensure "/" serves index.html from the local root.
    let get_base = std::sync::Arc::clone(&base_dir);
    let get_backend = std::sync::Arc::clone(&backend_arc);
    router.get("/*proxy_path", overlay_handler(get_base, get_backend));

    let root_base = std::sync::Arc::clone(&base_dir);
    let root_backend = std::sync::Arc::clone(&backend_arc);
    router.get("/", overlay_handler(root_base, root_backend));

    let head_base = std::sync::Arc::clone(&base_dir);
    let head_backend = std::sync::Arc::clone(&backend_arc);
    router.head("/*proxy_path", overlay_handler(head_base, head_backend));

    router.head("/", overlay_handler(base_dir, backend_arc));

    Ok(())
}
