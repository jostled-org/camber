use std::sync::Arc;

use rustls::pki_types::pem::PemObject;

pub fn generate_self_signed_cert() -> (Vec<u8>, Vec<u8>) {
    generate_cert_with_san("localhost")
}

pub fn generate_cert_with_san(san: &str) -> (Vec<u8>, Vec<u8>) {
    let cert = rcgen::generate_simple_self_signed(vec![san.to_owned()]).unwrap();
    (
        cert.cert.pem().into_bytes(),
        cert.signing_key.serialize_pem().into_bytes(),
    )
}

pub fn certified_key_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> rustls::sign::CertifiedKey {
    let (certs, key) = parse_pem(cert_pem, key_pem);
    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key).unwrap();
    rustls::sign::CertifiedKey::new(certs, signing_key)
}

pub fn build_server_config(cert_pem: &[u8], key_pem: &[u8]) -> Arc<rustls::ServerConfig> {
    let (certs, key) = parse_pem(cert_pem, key_pem);
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    Arc::new(config)
}

pub fn server_tls_config(cert_pem: &[u8], key_pem: &[u8]) -> Arc<rustls::ServerConfig> {
    let certified = certified_key_from_pem(cert_pem, key_pem);
    let store = camber::CertStore::new(certified);
    camber::tls::build_tls_config_from_resolver(store).unwrap()
}

pub fn tls_client_config(cert_pems: &[&[u8]]) -> rustls::ClientConfig {
    let mut root_store = rustls::RootCertStore::empty();
    cert_pems.iter().for_each(|pem| {
        rustls::pki_types::CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .for_each(|cert| root_store.add(cert).unwrap());
    });
    rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(root_store)
    .with_no_client_auth()
}

fn parse_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
) {
    let certs = rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem).unwrap();
    (certs, key)
}
