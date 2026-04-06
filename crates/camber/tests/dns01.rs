use camber::secret::{SecretRef, load_secret};
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn loads_token_from_env_var() {
    let expected = std::env::var("HOME").expect("HOME must be set");

    let result = load_secret(&SecretRef::Env("HOME".into()));

    assert_eq!(&*result.unwrap(), expected.trim());
}

#[test]
fn loads_token_from_file() {
    let mut f = NamedTempFile::new().expect("temp file");
    f.write_all(b"secret456\n").expect("write");

    let path: Box<str> = f.path().to_str().expect("utf8 path").into();
    let result = load_secret(&SecretRef::File(path));

    assert_eq!(&*result.unwrap(), "secret456");
}

#[test]
fn missing_env_var_returns_error() {
    let result = load_secret(&SecretRef::Env("NONEXISTENT_VAR_12345".into()));

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("NONEXISTENT_VAR_12345"),
        "error should name the variable: {msg}"
    );
}

#[test]
fn missing_file_returns_error() {
    let result = load_secret(&SecretRef::File("/tmp/nonexistent_token_file".into()));

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("nonexistent_token_file"),
        "error should name the file: {msg}"
    );
}

#[cfg(feature = "dns01")]
mod cloudflare {
    use camber::dns01::{CloudflareProvider, DnsProvider};
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn zone_response() -> serde_json::Value {
        json!({
            "success": true,
            "result": [{"id": "zone123", "name": "example.com"}],
            "errors": []
        })
    }

    async fn mock_zone_lookup(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(zone_response()))
            .mount(server)
            .await;
    }

    async fn setup_provider(server: &MockServer) -> CloudflareProvider {
        mock_zone_lookup(server).await;
        CloudflareProvider::with_base_url("test-token".into(), "example.com", server.uri().into())
            .await
            .expect("provider creation")
    }

    #[tokio::test]
    async fn cloudflare_creates_txt_record() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/zones/zone123/dns_records"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {"id": "record456"},
                "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = setup_provider(&server).await;
        let record_id = provider
            .create_txt_record("_acme-challenge.example.com", "token123")
            .await
            .expect("create record");

        assert_eq!(&*record_id, "record456");
    }

    #[tokio::test]
    async fn cloudflare_deletes_txt_record() {
        let server = MockServer::start().await;

        Mock::given(method("DELETE"))
            .and(path("/zones/zone123/dns_records/record456"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": {"id": "record456"},
                "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = setup_provider(&server).await;
        provider
            .delete_txt_record("record456")
            .await
            .expect("delete record");
    }

    #[tokio::test]
    async fn cloudflare_looks_up_zone_id() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [{"id": "resolved-zone-42", "name": "example.com"}],
                "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = CloudflareProvider::with_base_url(
            "test-token".into(),
            "example.com",
            server.uri().into(),
        )
        .await;

        assert!(
            provider.is_ok(),
            "zone ID should be resolved from API response"
        );
    }

    #[tokio::test]
    async fn cloudflare_zone_lookup_walks_hierarchy() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "app.example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [],
                "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "example.com"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [{"id": "zone-walked", "name": "example.com"}],
                "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = CloudflareProvider::with_base_url(
            "test-token".into(),
            "app.example.com",
            server.uri().into(),
        )
        .await;

        assert!(provider.is_ok(), "should find zone by walking hierarchy");
    }

    #[tokio::test]
    async fn cloudflare_zone_lookup_multi_part_tld() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "app.mysite.co.uk"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true, "result": [], "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/zones"))
            .and(query_param("name", "mysite.co.uk"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "result": [{"id": "zone-uk", "name": "mysite.co.uk"}],
                "errors": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let provider = CloudflareProvider::with_base_url(
            "test-token".into(),
            "app.mysite.co.uk",
            server.uri().into(),
        )
        .await;

        assert!(provider.is_ok(), "should find zone for multi-part TLD");
    }

    #[tokio::test]
    async fn cloudflare_auth_failure_returns_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/zones/zone123/dns_records"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false,
                "result": null,
                "errors": [{"code": 9103, "message": "Unknown X-Auth-Key or X-Auth-Email"}]
            })))
            .mount(&server)
            .await;

        let provider = setup_provider(&server).await;
        let err = provider
            .create_txt_record("_acme-challenge.example.com", "token123")
            .await
            .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("Auth"),
            "error should describe authentication failure: {msg}"
        );
    }
}

#[cfg(feature = "dns01")]
mod acme_dns01 {
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use camber::dns01::AcmeDns01;
    use tempfile::TempDir;

    fn write_test_cache(cache_dir: &Path, days_until_expiry: i64) {
        let ck = rcgen::generate_simple_self_signed(vec!["test.example.com".into()])
            .expect("generate cert");

        std::fs::create_dir_all(cache_dir).expect("create cache dir");
        std::fs::write(cache_dir.join("cert.pem"), ck.cert.pem()).expect("write cert");
        std::fs::write(cache_dir.join("key.pem"), ck.key_pair.serialize_pem()).expect("write key");

        let expiry_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs() as i64
            + (days_until_expiry * 86400);
        std::fs::write(cache_dir.join("expiry"), expiry_secs.to_string()).expect("write expiry");
    }

    #[tokio::test]
    #[ignore]
    async fn acme_dns01_provisions_cert() {
        let domain = std::env::var("ACME_TEST_DOMAIN").expect("ACME_TEST_DOMAIN must be set");
        let token = std::env::var("CF_TOKEN").expect("CF_TOKEN must be set");

        let provider = camber::dns01::CloudflareProvider::new(token.into(), &domain)
            .await
            .expect("cloudflare provider");

        let cache_dir = TempDir::new().expect("temp dir");
        let config = AcmeDns01::new("test", [&*domain])
            .email("test@example.com")
            .cache_dir(cache_dir.path())
            .staging(true);

        let cert = config
            .provision_cert(&provider)
            .await
            .expect("provision cert");
        assert!(!cert.cert.is_empty(), "cert chain should not be empty");
        assert!(
            cache_dir.path().join("cert.pem").exists(),
            "cert cached to disk"
        );
        assert!(
            cache_dir.path().join("key.pem").exists(),
            "key cached to disk"
        );
    }

    #[tokio::test]
    async fn cert_cached_to_disk() {
        let cache_dir = TempDir::new().expect("temp dir");
        write_test_cache(cache_dir.path(), 60);

        let config = AcmeDns01::new("test", ["test.example.com"]).cache_dir(cache_dir.path());
        let cert = config.load_cached_cert().expect("load cached cert");
        assert!(
            cert.is_some(),
            "cached cert should load without new ACME order"
        );

        let config2 = AcmeDns01::new("test", ["test.example.com"]).cache_dir(cache_dir.path());
        let cert2 = config2.load_cached_cert().expect("load cached cert again");
        assert!(cert2.is_some(), "cert still loadable from cache");
    }

    #[tokio::test]
    async fn renewal_triggered_before_expiry() {
        let cache_near = TempDir::new().expect("temp dir");
        write_test_cache(cache_near.path(), 15);
        let config_near = AcmeDns01::new("test", ["test.example.com"]).cache_dir(cache_near.path());
        assert!(
            config_near.needs_renewal(),
            "cert expiring in 15 days should need renewal"
        );

        let cache_far = TempDir::new().expect("temp dir");
        write_test_cache(cache_far.path(), 60);
        let config_far = AcmeDns01::new("test", ["test.example.com"]).cache_dir(cache_far.path());
        assert!(
            !config_far.needs_renewal(),
            "cert expiring in 60 days should not need renewal"
        );
    }
}
