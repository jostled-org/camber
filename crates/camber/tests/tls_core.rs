mod common;

use camber::tls::{CertStore, load_certified_key, resolve_tls};

#[test]
fn cert_store_resolves_cert() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let key = common::certified_key_from_pem(&cert_pem, &key_pem);
    let store = CertStore::new(key);

    let loaded = store.load();
    assert!(!loaded.cert.is_empty());
}

#[test]
fn cert_store_hot_swap() {
    let (cert_a_pem, key_a_pem) = common::generate_self_signed_cert();
    let (cert_b_pem, key_b_pem) = common::generate_self_signed_cert();

    let key_a = common::certified_key_from_pem(&cert_a_pem, &key_a_pem);
    let key_b = common::certified_key_from_pem(&cert_b_pem, &key_b_pem);

    let expected_b_cert = key_b.cert[0].as_ref().to_vec();
    let store = CertStore::new(key_a);

    store.swap(key_b);

    let loaded = store.load();
    assert_eq!(loaded.cert[0].as_ref(), &expected_b_cert);
}

#[test]
fn load_certified_key_from_pem_files() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    let result = load_certified_key(&cert_path, &key_path);
    assert!(result.is_ok());
}

#[test]
fn load_certified_key_missing_file_returns_error() {
    let result = load_certified_key(
        std::path::Path::new("/nonexistent/cert.pem"),
        std::path::Path::new("/nonexistent/key.pem"),
    );
    assert!(result.is_err());
}

#[test]
fn resolve_tls_with_cert_store() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let key = common::certified_key_from_pem(&cert_pem, &key_pem);
    let store = CertStore::new(key);

    let (tls_cfg, cert_store) = resolve_tls(Some(store), None, None).unwrap();
    assert!(tls_cfg.is_some());
    assert!(cert_store.is_some());
}

#[test]
fn resolve_tls_with_pem_paths() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    let (tls_cfg, cert_store) = resolve_tls(None, Some(cert_path), Some(key_path)).unwrap();
    assert!(tls_cfg.is_some());
    assert!(cert_store.is_some());
}

#[test]
fn resolve_tls_with_nothing_returns_none() {
    let (tls_cfg, cert_store) = resolve_tls(None, None, None).unwrap();
    assert!(tls_cfg.is_none());
    assert!(cert_store.is_none());
}

#[test]
fn resolve_tls_partial_returns_error() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, _key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    let result = resolve_tls(None, Some(cert_path), None);
    assert!(result.is_err());
}
