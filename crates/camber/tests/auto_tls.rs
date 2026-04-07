#![cfg(feature = "acme")]

use camber::acme::AcmeConfig;
use rustls::pki_types::pem::PemObject;
use rustls_acme::CertCache;
use rustls_acme::caches::DirCache;

/// Run an async block inside the Camber runtime using block_in_place.
fn run_async<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Write a test cert to the DirCache and read it back. Verifies that the
/// filesystem cache layer (used by rustls-acme) does a faithful round-trip.
#[test]
fn filesystem_cache_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DirCache::new(tmp.path().to_path_buf());

    let domains = vec!["example.com".to_string()];
    let directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory";
    let fake_cert = b"-----BEGIN FAKE CERT-----\ntest data\n-----END FAKE CERT-----\n";

    camber::runtime::test(|| {
        run_async(async {
            cache
                .store_cert(&domains, directory_url, fake_cert)
                .await
                .unwrap();
            let loaded = cache.load_cert(&domains, directory_url).await.unwrap();
            assert_eq!(loaded.as_deref(), Some(&fake_cert[..]));
        });
    })
    .unwrap();
}

/// Pre-populate a temp cache dir, then verify a fresh DirCache pointed at the
/// same directory can load the cert. Confirms cache persistence across restarts.
#[test]
fn filesystem_cache_loads_on_startup() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DirCache::new(tmp.path().to_path_buf());

    let domains = vec!["startup.example.com".to_string()];
    let directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory";
    let fake_cert = b"pre-populated cert data";

    // Pre-populate the cache (simulates a previous server run).
    camber::runtime::test(|| {
        run_async(async {
            cache
                .store_cert(&domains, directory_url, fake_cert)
                .await
                .unwrap();
        });
    })
    .unwrap();

    // Verify AcmeConfig builder plumbing works.
    let _config = AcmeConfig::new("camber", ["startup.example.com"])
        .cache_dir(tmp.path())
        .staging(true);

    // A fresh DirCache at the same path can load the cert — simulates restart.
    let cache2 = DirCache::new(tmp.path().to_path_buf());
    camber::runtime::test(|| {
        run_async(async {
            let loaded = cache2.load_cert(&domains, directory_url).await.unwrap();
            assert_eq!(
                loaded.as_deref(),
                Some(&fake_cert[..]),
                "cert should be loadable from the pre-populated cache"
            );
        });
    })
    .unwrap();
}

/// Pre-populate the ACME cert cache with a self-signed cert, then start a
/// server with tls_auto(). Verify the cached cert is served over HTTPS —
/// validates that certificates persist across restarts.
#[test]
fn cert_persists_across_restarts() {
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let cache = DirCache::new(tmp.path().to_path_buf());

    // Generate an ECDSA P256 self-signed cert (same type rustls-acme uses).
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert_pem = cert.cert.pem();
    let key_pem = cert.signing_key.serialize_pem();

    // rustls-acme expects: private key PEM first, then cert chain PEM.
    let cached_pem = format!("{key_pem}\n{cert_pem}");

    let domains = vec!["localhost".to_string()];
    let directory_url = "https://acme-staging-v02.api.letsencrypt.org/directory";

    // Pre-populate the cache (simulates a previous ACME provisioning).
    camber::runtime::test(|| {
        run_async(async {
            cache
                .store_cert(&domains, directory_url, cached_pem.as_bytes())
                .await
                .unwrap();
        });
    })
    .unwrap();

    // Start server with tls_auto pointing to the pre-populated cache.
    let acme_config = AcmeConfig::new("camber", ["localhost"])
        .email("test@example.com")
        .cache_dir(tmp.path())
        .staging(true);

    let cert_pem_bytes = cert_pem.into_bytes();

    camber::runtime::builder()
        .keepalive_timeout(std::time::Duration::from_millis(200))
        .shutdown_timeout(std::time::Duration::from_secs(2))
        .tls_auto(acme_config)
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/cached", |_req: &camber::http::Request| async {
                camber::http::Response::text(200, "from cache")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Give the ACME renewal task time to load the cached cert.
            std::thread::sleep(std::time::Duration::from_millis(500));

            // Build a TLS client that trusts our self-signed cert.
            let mut root_store = rustls::RootCertStore::empty();
            let certs = rustls::pki_types::CertificateDer::pem_slice_iter(&cert_pem_bytes)
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            for cert in certs {
                root_store.add(cert).unwrap();
            }
            let client_config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(root_store)
            .with_no_client_auth();

            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

            let status = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                    let tls_stream = connector.connect(server_name, tcp).await.unwrap();

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let (mut sender, conn) =
                        hyper::client::conn::http1::handshake(io).await.unwrap();
                    tokio::spawn(conn);

                    let req = hyper::Request::get("http://localhost/cached")
                        .body(http_body_util::Empty::<bytes::Bytes>::new())
                        .unwrap();
                    let resp = sender.send_request(req).await.unwrap();
                    resp.status().as_u16()
                })
            });

            assert_eq!(status, 200);

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

/// tls_auto() and tls_cert()/tls_key() are mutually exclusive.
#[test]
fn tls_auto_rejects_combined_with_manual_cert() {
    let tmp = tempfile::tempdir().unwrap();
    let cert_path = tmp.path().join("cert.pem");
    let key_path = tmp.path().join("key.pem");

    // Write dummy files so path validation doesn't fail first.
    std::fs::write(&cert_path, b"not a real cert").unwrap();
    std::fs::write(&key_path, b"not a real key").unwrap();

    let acme_config = AcmeConfig::new("camber", ["example.com"])
        .email("test@test.com")
        .cache_dir(tmp.path())
        .staging(true);

    let result = camber::runtime::builder()
        .tls_auto(acme_config)
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {});

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("mutually exclusive"),
        "expected mutual exclusion error, got: {err}"
    );
}

/// tls_auto() produces a valid builder (config construction succeeds).
#[test]
fn tls_auto_builds_config() {
    let tmp = tempfile::tempdir().unwrap();

    let acme_config = AcmeConfig::new("camber", ["example.com"])
        .email("admin@example.com")
        .cache_dir(tmp.path())
        .staging(true);

    // run should succeed in building the ACME config, even though
    // the ACME provisioning won't actually complete (no real ACME server).
    // The closure returns immediately and requests shutdown.
    let result = camber::runtime::builder()
        .shutdown_timeout(std::time::Duration::from_secs(1))
        .tls_auto(acme_config)
        .run(|| {
            camber::runtime::request_shutdown();
        });

    assert!(result.is_ok(), "tls_auto build failed: {result:?}");
}
