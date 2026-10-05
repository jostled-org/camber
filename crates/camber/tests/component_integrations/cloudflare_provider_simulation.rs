//! 10.T1: the Cloudflare descriptor is inert, and preparation authorizes
//! every configured zone before a TXT record is written.
//!
//! Every row enters through the public `camber::dns01` API. The actual
//! reqwest transport reaches a controlled local provider that records each
//! request, so a row counts what reached the wire. Rows run independently
//! and the matrix reports every broken claim together.

use crate::integration_rows::{
    Refusal, Row, all, assert_verdicts, expect, expect_eq, expect_refused, invalid_config,
    limit_exceeded, permission_denied, refusal, refused, rejected,
};
use camber::dns01::{CloudflareProvider, DnsProvider};
use camber::{IntegrationOperation, RuntimeError};
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "test-token";

/// The largest Cloudflare body the provider reads before refusing it.
const BODY_LIMIT: usize = 1024 * 1024;

fn names(domains: &[&str]) -> Box<[Arc<str>]> {
    domains.iter().map(|domain| Arc::from(*domain)).collect()
}

fn descriptor(server: &MockServer) -> Result<CloudflareProvider, String> {
    CloudflareProvider::with_base_url(TOKEN.into(), server.uri().into_boxed_str())
        .map_err(|error| format!("descriptor refused: {error:?}"))
}

fn zones_body(zones: &[(&str, &str)]) -> serde_json::Value {
    let result: Vec<_> = zones
        .iter()
        .map(|(name, id)| json!({"id": id, "name": name}))
        .collect();
    json!({"success": true, "result": result, "errors": []})
}

/// A lookup of `query` answered with `zones`.
fn zones_mock(query: &str, zones: &[(&str, &str)]) -> Mock {
    Mock::given(method("GET"))
        .and(path("/zones"))
        .and(query_param("name", query))
        .respond_with(ResponseTemplate::new(200).set_body_json(zones_body(zones)))
}

/// An authorized `verb` to `route`, answered with `record`.
fn record_mock(verb: &str, route: String, record: &str) -> Mock {
    Mock::given(method(verb))
        .and(path(route))
        .and(header("Authorization", format!("Bearer {TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true, "result": {"id": record}, "errors": []
        })))
}

fn create_mock(zone: &str, record: &str) -> Mock {
    record_mock("POST", format!("/zones/{zone}/dns_records"), record)
}

fn delete_mock(zone: &str, record: &str) -> Mock {
    record_mock(
        "DELETE",
        format!("/zones/{zone}/dns_records/{record}"),
        record,
    )
}

async fn mount_zones(server: &MockServer, query: &str, zones: &[(&str, &str)]) {
    zones_mock(query, zones).mount(server).await;
}

async fn mount_create(server: &MockServer, zone: &str, record: &str) {
    create_mock(zone, record).mount(server).await;
}

async fn mount_delete(server: &MockServer, zone: &str, record: &str) {
    delete_mock(zone, record).mount(server).await;
}

/// Every request the provider sent, as method and path with query.
async fn requests(server: &MockServer) -> Result<Vec<String>, String> {
    let received = server
        .received_requests()
        .await
        .ok_or("the provider peer records no requests")?;
    Ok(received
        .iter()
        .map(|request| {
            let query = request
                .url
                .query()
                .map(|query| format!("?{query}"))
                .unwrap_or_default();
            format!("{} {}{query}", request.method, request.url.path())
        })
        .collect())
}

async fn writes(server: &MockServer) -> Result<usize, String> {
    Ok(requests(server)
        .await?
        .iter()
        .filter(|request| !request.starts_with("GET "))
        .count())
}

const REJECTED_LOOKUP: Refusal = rejected(IntegrationOperation::ZoneLookup);

/// Constructing and dropping descriptors sends nothing, needs no runtime,
/// and refuses invalid explicit inputs as configuration.
async fn construction_is_pure() -> Row {
    let server = MockServer::start().await;
    let base = server.uri();
    let built = std::thread::spawn(move || {
        let production = CloudflareProvider::new(TOKEN.into()).map(drop);
        let local = CloudflareProvider::with_base_url(TOKEN.into(), base.into()).map(drop);
        (production, local)
    })
    .join()
    .map_err(|_| "constructing a descriptor panicked".to_owned())?;
    let invalid = |token: &str, base: &str| {
        refused(CloudflareProvider::with_base_url(token.into(), base.into()))
    };
    let config = Some(invalid_config(IntegrationOperation::ZoneLookup));
    all([
        expect("the production descriptor is accepted", built.0.is_ok()),
        expect("the loopback descriptor is accepted", built.1.is_ok()),
        expect_eq(
            "an empty token",
            invalid("", "https://api.example.com"),
            config,
        ),
        expect_eq(
            "a token with whitespace",
            invalid("to ken", "https://api.example.com"),
            config,
        ),
        expect_eq(
            "a non-HTTP base",
            invalid(TOKEN, "ftp://api.example.com"),
            config,
        ),
        expect_eq(
            "plain HTTP to a remote host",
            invalid(TOKEN, "http://api.example.com"),
            config,
        ),
        expect_eq(
            "credentials in the base",
            invalid(TOKEN, "https://user:pass@api.example.com"),
            config,
        ),
        expect_eq(
            "a query in the base",
            invalid(TOKEN, "https://api.example.com/v4?zone=1"),
            config,
        ),
        expect_eq("a relative base", invalid(TOKEN, "/client/v4"), config),
        expect_eq(
            "requests from constructing and dropping",
            requests(&server).await?,
            Vec::<String>::new(),
        ),
    ])
}

/// Each domain resolves its own zone at the longest matching suffix; a
/// wildcard is stripped only for selection; creates and deletes reach the
/// zone their name belongs to, by exact record ID.
async fn every_domain_resolves_its_own_zone() -> Row {
    let server = MockServer::start().await;
    mount_zones(&server, "app.bar.example.com", &[]).await;
    mount_zones(
        &server,
        "bar.example.com",
        &[("bar.example.com", "zone-bar")],
    )
    .await;
    mount_zones(
        &server,
        "other.example.org",
        &[("other.example.org", "zone-org")],
    )
    .await;
    mount_create(&server, "zone-bar", "record-bar").await;
    mount_create(&server, "zone-org", "record-org").await;
    mount_delete(&server, "zone-bar", "record-bar").await;
    mount_delete(&server, "zone-org", "record-org").await;
    let mut provider = descriptor(&server)?;
    provider
        .prepare(&names(&["app.bar.example.com", "*.other.example.org"]))
        .await
        .map_err(|error| format!("prepare: {error:?}"))?;
    let lookups = requests(&server).await?;
    let bar = provider
        .create_txt_record("_acme-challenge.app.bar.example.com", "value-bar")
        .await;
    let org = provider
        .create_txt_record("_acme-challenge.other.example.org", "value-org")
        .await;
    let deleted_bar = provider.delete_txt_record("record-bar").await;
    let deleted_org = provider.delete_txt_record("record-org").await;
    all([
        expect_eq(
            "zone queries, longest suffix first",
            lookups,
            vec![
                "GET /zones?name=app.bar.example.com".to_owned(),
                "GET /zones?name=bar.example.com".to_owned(),
                "GET /zones?name=other.example.org".to_owned(),
            ],
        ),
        expect_eq(
            "the bar record",
            bar.map_err(|error| format!("{error:?}")).as_deref(),
            Ok("record-bar"),
        ),
        expect_eq(
            "the org record",
            org.map_err(|error| format!("{error:?}")).as_deref(),
            Ok("record-org"),
        ),
        expect("the bar delete", deleted_bar.is_ok()),
        expect("the org delete", deleted_org.is_ok()),
        expect_eq(
            "writes in zone order",
            requests(&server).await?.split_off(3),
            vec![
                "POST /zones/zone-bar/dns_records".to_owned(),
                "POST /zones/zone-org/dns_records".to_owned(),
                "DELETE /zones/zone-bar/dns_records/record-bar".to_owned(),
                "DELETE /zones/zone-org/dns_records/record-org".to_owned(),
            ],
        ),
    ])
}

/// A wildcard and its base share one challenge name, so preparing both
/// resolves that zone once, and the shared name still reaches it.
async fn wildcard_and_base_share_one_lookup() -> Row {
    let server = MockServer::start().await;
    mount_zones(&server, "example.com", &[("example.com", "zone-shared")]).await;
    mount_create(&server, "zone-shared", "record-shared").await;
    let mut provider = descriptor(&server)?;
    provider
        .prepare(&names(&["example.com", "*.Example.com"]))
        .await
        .map_err(|error| format!("prepare: {error:?}"))?;
    let lookups = requests(&server).await?;
    let created = provider
        .create_txt_record("_acme-challenge.example.com", "value-shared")
        .await;
    all([
        expect_eq(
            "zone queries",
            lookups,
            vec!["GET /zones?name=example.com".to_owned()],
        ),
        expect_eq(
            "the shared record",
            created.map_err(|error| format!("{error:?}")).as_deref(),
            Ok("record-shared"),
        ),
    ])
}

/// Prepare one domain against a peer answering every lookup with
/// `template`, and expect the refusal `expected`.
async fn lookup_refusal(template: ResponseTemplate, expected: Refusal, what: &str) -> Row {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/zones"))
        .respond_with(template)
        .mount(&server)
        .await;
    let mut provider = descriptor(&server)?;
    let prepared = provider.prepare(&names(&["app.example.com"])).await;
    expect_refused(what, prepared, expected)
}

/// Two matching zones, a zone of another name, and an empty ID each
/// refuse rather than fall back to a broader zone.
async fn ambiguous_or_mismatched_zones_refuse() -> Row {
    let two = ResponseTemplate::new(200).set_body_json(zones_body(&[
        ("app.example.com", "zone-a"),
        ("app.example.com", "zone-b"),
    ]));
    let mismatched =
        ResponseTemplate::new(200).set_body_json(zones_body(&[("example.com", "zone-parent")]));
    let empty_id = ResponseTemplate::new(200).set_body_json(zones_body(&[("app.example.com", "")]));
    all([
        lookup_refusal(two, REJECTED_LOOKUP, "two matching zones").await,
        lookup_refusal(mismatched, REJECTED_LOOKUP, "a zone of another name").await,
        lookup_refusal(empty_id, REJECTED_LOOKUP, "an empty zone ID").await,
    ])
}

/// Permission, malformed, and oversized answers are typed by status and
/// structure, never by provider text.
async fn answers_are_classified_structurally() -> Row {
    let forbidden = ResponseTemplate::new(403).set_body_json(json!({
        "success": false, "result": null,
        "errors": [{"code": 9109, "message": "Invalid access token"}]
    }));
    let auth_code = ResponseTemplate::new(400).set_body_json(json!({
        "success": false, "result": null,
        "errors": [{"code": 10000, "message": "Authentication error"}]
    }));
    let malformed = ResponseTemplate::new(200).set_body_string("{ not json");
    let oversized = ResponseTemplate::new(200).set_body_string(format!(
        "{{\"success\":true,\"result\":[],\"errors\":[],\"pad\":\"{}\"}}",
        "x".repeat(BODY_LIMIT)
    ));
    let redirect = ResponseTemplate::new(307).insert_header("Location", "/elsewhere");
    let permission = permission_denied(IntegrationOperation::ZoneLookup);
    all([
        lookup_refusal(forbidden, permission, "HTTP 403").await,
        lookup_refusal(auth_code, permission, "an authentication error code").await,
        lookup_refusal(malformed, REJECTED_LOOKUP, "a malformed body").await,
        lookup_refusal(
            oversized,
            limit_exceeded(IntegrationOperation::ZoneLookup),
            "a body over 1 MiB",
        )
        .await,
        lookup_refusal(redirect, REJECTED_LOOKUP, "a redirect").await,
    ])
}

/// A lookup that fails partway publishes no authority: the domain that
/// did resolve cannot be written either.
async fn failed_partial_map_is_not_published() -> Row {
    let server = MockServer::start().await;
    mount_zones(
        &server,
        "good.example.com",
        &[("good.example.com", "zone-good")],
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/zones"))
        .and(query_param("name", "bad.example.org"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "success": false, "result": null, "errors": [{"code": 9109}]
        })))
        .mount(&server)
        .await;
    mount_create(&server, "zone-good", "record-good").await;
    let mut provider = descriptor(&server)?;
    let prepared = provider
        .prepare(&names(&["good.example.com", "bad.example.org"]))
        .await;
    let create = provider
        .create_txt_record("_acme-challenge.good.example.com", "value")
        .await;
    all([
        expect_refused(
            "the failed lookup",
            prepared,
            permission_denied(IntegrationOperation::ZoneLookup),
        ),
        expect_refused(
            "a create after the failed preparation",
            create,
            rejected(IntegrationOperation::CreateTxt),
        ),
        expect_eq("writes sent", writes(&server).await?, 0),
    ])
}

/// A create or delete outside the prepared authority, or before any
/// preparation, is refused before it is sent.
async fn names_outside_authority_are_refused_before_sending() -> Row {
    let server = MockServer::start().await;
    mount_zones(&server, "example.com", &[("example.com", "zone-1")]).await;
    mount_create(&server, "zone-1", "record-1").await;
    let unprepared = descriptor(&server)?
        .create_txt_record("_acme-challenge.example.com", "value")
        .await;
    let mut provider = descriptor(&server)?;
    provider
        .prepare(&names(&["example.com"]))
        .await
        .map_err(|error| format!("prepare: {error:?}"))?;
    let outside = provider
        .create_txt_record("_acme-challenge.evil.example.net", "value")
        .await;
    let sibling = provider
        .create_txt_record("_acme-challenge.www.example.com", "value")
        .await;
    let not_challenge = provider.create_txt_record("example.com", "value").await;
    let unknown_delete = provider.delete_txt_record("record-unrelated").await;
    let create = rejected(IntegrationOperation::CreateTxt);
    all([
        expect_refused("a create before preparation", unprepared, create),
        expect_refused("a create in another domain", outside, create),
        expect_refused("a create for an unconfigured name", sibling, create),
        expect_refused(
            "a create outside the challenge label",
            not_challenge,
            create,
        ),
        expect_refused(
            "a delete of a record this order did not create",
            unknown_delete,
            rejected(IntegrationOperation::DeleteTxt),
        ),
        expect_eq("writes sent", writes(&server).await?, 0),
    ])
}

#[tokio::test]
async fn cloudflare_descriptor_is_pure_and_preparation_authorizes_every_zone() {
    let rows = [
        ("construction is pure", construction_is_pure().await),
        (
            "every domain resolves its own zone",
            every_domain_resolves_its_own_zone().await,
        ),
        (
            "a wildcard and its base share one lookup",
            wildcard_and_base_share_one_lookup().await,
        ),
        (
            "ambiguous or mismatched zones refuse",
            ambiguous_or_mismatched_zones_refuse().await,
        ),
        (
            "answers are classified structurally",
            answers_are_classified_structurally().await,
        ),
        (
            "a failed partial map is not published",
            failed_partial_map_is_not_published().await,
        ),
        (
            "names outside the authority are refused before sending",
            names_outside_authority_are_refused_before_sending().await,
        ),
    ];
    assert_verdicts("Cloudflare preparation", rows);
}

async fn setup_provider(server: &MockServer) -> CloudflareProvider {
    mount_zones(server, "example.com", &[("example.com", "zone123")]).await;
    let mut provider = descriptor(server).expect("provider descriptor");
    provider
        .prepare(&names(&["example.com"]))
        .await
        .expect("provider preparation");
    provider
}

#[tokio::test]
async fn cloudflare_creates_txt_record() {
    let server = MockServer::start().await;
    create_mock("zone123", "record456")
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
    mount_create(&server, "zone123", "record456").await;
    delete_mock("zone123", "record456")
        .expect(1)
        .mount(&server)
        .await;

    let provider = setup_provider(&server).await;
    let record_id = provider
        .create_txt_record("_acme-challenge.example.com", "token123")
        .await
        .expect("create record");
    provider
        .delete_txt_record(&record_id)
        .await
        .expect("delete record");
}

#[tokio::test]
async fn cloudflare_looks_up_zone_id() {
    let server = MockServer::start().await;
    zones_mock("example.com", &[("example.com", "resolved-zone-42")])
        .expect(1)
        .mount(&server)
        .await;
    mount_create(&server, "resolved-zone-42", "record-42").await;

    let mut provider = descriptor(&server).expect("provider descriptor");
    provider
        .prepare(&names(&["example.com"]))
        .await
        .expect("zone ID should be resolved from API response");
    let record = provider
        .create_txt_record("_acme-challenge.example.com", "token")
        .await
        .expect("the resolved zone receives the record");
    assert_eq!(&*record, "record-42");
}

/// Prepare `domain` against a peer that knows only `zone`, after one
/// empty lookup of `domain` itself.
async fn walk_to_zone(domain: &str, zone: (&str, &str)) -> Result<(), RuntimeError> {
    let server = MockServer::start().await;
    zones_mock(domain, &[]).expect(1).mount(&server).await;
    zones_mock(zone.0, &[zone]).expect(1).mount(&server).await;
    let mut provider = descriptor(&server).expect("provider descriptor");
    provider.prepare(&names(&[domain])).await
}

#[tokio::test]
async fn cloudflare_zone_lookup_walks_hierarchy() {
    let prepared = walk_to_zone("app.example.com", ("example.com", "zone-walked")).await;

    assert!(prepared.is_ok(), "should find zone by walking hierarchy");
}

#[tokio::test]
async fn cloudflare_zone_lookup_multi_part_tld() {
    let prepared = walk_to_zone("app.mysite.co.uk", ("mysite.co.uk", "zone-uk")).await;

    assert!(prepared.is_ok(), "should find zone for multi-part TLD");
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
    let error = provider
        .create_txt_record("_acme-challenge.example.com", "token123")
        .await
        .unwrap_err();

    assert_eq!(
        refusal(&error),
        Some(permission_denied(IntegrationOperation::CreateTxt)),
        "error should describe authentication failure: {error:?}"
    );
}
