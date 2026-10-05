#![cfg(feature = "dns01")]

//! 9.T2: a validated cache generation reaches TLS, and again after a restart.
//!
//! `AcmeDns01::load_cached_cert` reads a legacy pair, validates it, and
//! migrates it. Its key goes into a `CertStore` through the public
//! `tls_resolver` builder, and a client that trusts only that leaf completes a
//! hostname-verified handshake. The second runtime reads the migrated bundle
//! after the legacy pair is gone. This proves the reader's output, not DNS
//! startup.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use camber::dns01::AcmeDns01;
use camber::http::{Request, Response, Router};
use camber::tls::CertStore;
use camber::{JoinHandle, RuntimeError, runtime, spawn};
use tempfile::TempDir;

use crate::common::{block_on, https_get, tls_client_config};

const HOST: &str = "localhost";
/// Bounds each HTTPS exchange so a hung handshake fails the test.
const EXCHANGE_BOUND: Duration = Duration::from_secs(5);

#[test]
fn validated_cache_generation_survives_tls_restart() {
    let cache = TempDir::new().expect("cache directory");
    let generated =
        rcgen::generate_simple_self_signed(vec![HOST.to_owned()]).expect("generate leaf");
    let cert_pem = generated.cert.pem();
    std::fs::write(cache.path().join("cert.pem"), &cert_pem).expect("seed legacy certificate");
    std::fs::write(
        cache.path().join("key.pem"),
        generated.signing_key.serialize_pem(),
    )
    .expect("seed legacy key");

    let first = serve_cached_generation(cache.path(), cert_pem.as_bytes());
    assert_eq!(&*first, "cached", "first runtime");

    std::fs::remove_file(cache.path().join("cert.pem")).expect("remove legacy certificate");
    std::fs::remove_file(cache.path().join("key.pem")).expect("remove legacy key");

    let restarted = serve_cached_generation(cache.path(), cert_pem.as_bytes());
    assert_eq!(&*restarted, "cached", "restarted runtime");
}

/// Run one runtime whose TLS store holds the cache reader's key, and return the
/// body a client that trusts only `trusted_pem` reads over HTTPS.
fn serve_cached_generation(cache_dir: &Path, trusted_pem: &[u8]) -> Box<str> {
    let key = AcmeDns01::new("camber-test", [HOST])
        .cache_dir(cache_dir)
        .load_cached_cert()
        .expect("cache read")
        .expect("a cached generation");
    let store = CertStore::new(key);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_client_config(&[trusted_pem])));

    runtime::builder()
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_resolver(store)
        .run(move || {
            let mut router = Router::new();
            router.get("/", |_: &Request| async { Response::text(200, "cached") });
            let (addr, server) = spawn_listener(router);
            let body = block_on(async {
                tokio::time::timeout(EXCHANGE_BOUND, https_get(&connector, addr, "/")).await
            })
            .expect("HTTPS exchange within bound")
            .expect("HTTPS exchange")
            .1;
            runtime::request_shutdown();
            server
                .join()
                .expect("join the TLS server")
                .expect("the TLS server returned cleanly");
            body
        })
        .expect("runtime")
}

/// Serve `router` on an ephemeral port, returning its address and the serve
/// call's handle, so its outcome is read rather than discarded.
fn spawn_listener(router: Router) -> (SocketAddr, JoinHandle<Result<(), RuntimeError>>) {
    let listener = camber::net::listen("127.0.0.1:0").expect("listen");
    let addr = listener
        .local_addr()
        .expect("local address")
        .tcp()
        .expect("tcp address");
    let server = spawn(move || -> Result<(), RuntimeError> {
        camber::http::serve_listener(listener, router)
    });
    (addr, server)
}
