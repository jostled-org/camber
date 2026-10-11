//! 3.T1, 3.T2 and 3.T3: one validated request description is what every
//! outbound call sends.
//!
//! Each wire row sends through the public client at a recording peer, which
//! keeps the exact head and body of every request it reads. A row collects its
//! results, stops and joins the peer, and only then asserts. The peer's record
//! is complete at that point, so a request that must not be sent is proved by
//! its absence from a joined record.

use crate::bounded_client::{assert_client_limit, within_bound};
use crate::error_chain::assert_safe;
use crate::retry_upstream::{Recorded, Recording, RecordingUpstream, answer, answered, failure};

use camber::__private::client_header_sensitivity;
use camber::RuntimeError;
use camber::http;
use camber::http::Bytes;
use camber::http::Client;
use camber::http::ClientError;
use camber::http::ClientInput;
use camber::http::ClientRequest;
use camber::http::ClientResponse;
use camber::http::Method;
use camber::http::Response;
use camber::http::mock;
use serde::{Deserialize, Serialize, Serializer};
use std::net::SocketAddr;

/// A 200 answer with no body, which every method may receive, HEAD included.
fn empty_answer() -> Vec<u8> {
    answer("200 OK", &[], b"")
}

/// One request description, as data.
///
/// The setters run in the order of the fields: the method and target, the
/// sensitive headers, the ordinary headers, the body, then the response
/// ceiling.
#[derive(Clone, Copy, Debug)]
struct Draft<'d> {
    method: &'d str,
    sensitive: &'d [(&'d str, &'d str)],
    headers: &'d [(&'d str, &'d str)],
    body: Option<&'d [u8]>,
    limit: Option<usize>,
}

/// A GET with no header, no body, and the client's own response ceiling.
const PLAIN: Draft<'static> = Draft {
    method: "GET",
    sensitive: &[],
    headers: &[],
    body: None,
    limit: None,
};

/// Build the request `draft` describes. The first setter that refuses its
/// input ends the build.
fn describe<'c>(
    client: &'c Client,
    url: &str,
    draft: Draft<'_>,
) -> Result<ClientRequest<'c>, RuntimeError> {
    let mut request: ClientRequest<'c> = client.request(draft.method, url)?;
    for (name, value) in draft.sensitive {
        request = request.sensitive_header(name, value)?;
    }
    for (name, value) in draft.headers {
        request = request.header(name, value)?;
    }
    if let Some(body) = draft.body {
        request = request.body(Bytes::copy_from_slice(body));
    }
    if let Some(limit) = draft.limit {
        request = request.response_limit(limit)?;
    }
    Ok(request)
}

/// Build the request `draft` describes and send it.
async fn submit(
    client: &Client,
    url: &str,
    draft: Draft<'_>,
) -> Result<ClientResponse, RuntimeError> {
    let request: ClientRequest<'_> = describe(client, url, draft)?;
    request.send().await
}

/// The status one call returned, or its failure.
fn status_of(result: Result<ClientResponse, RuntimeError>) -> Result<u16, RuntimeError> {
    result.map(|response| response.status())
}

/// Require every labelled call of a row to have answered 200.
fn assert_all_ok<L: std::fmt::Display>(row: &str, statuses: &[(L, Result<u16, RuntimeError>)]) {
    for (label, status) in statuses {
        assert!(
            matches!(status, Ok(200)),
            "{row}: {label} returned {status:?}"
        );
    }
}

/// Headers no request here asks for. A client that sends one added it itself.
const AMBIENT_HEADERS: [&str; 3] = ["referer", "cookie", "proxy-authorization"];

/// What one recorded request must have carried.
struct Sent<'a> {
    method: &'a str,
    target: &'a str,
    content_type: Option<&'a str>,
    body: &'a [u8],
}

/// Require one recorded request to carry exactly the expected method, target,
/// content type, and body, and no ambient header.
fn assert_sent(row: &str, sent: &Recorded, expected: &Sent<'_>) {
    assert_eq!(
        sent.method(),
        expected.method,
        "{row}: the peer read another method"
    );
    assert_eq!(
        sent.target(),
        expected.target,
        "{row}: the peer read another target"
    );
    assert_eq!(
        *sent.header_values("content-type"),
        *expected.content_type.as_slice(),
        "{row}: the peer read another content type"
    );
    assert_eq!(
        &*sent.body, expected.body,
        "{row}: the peer read another body"
    );
    for ambient in AMBIENT_HEADERS {
        assert!(
            sent.header_values(ambient).is_empty(),
            "{row}: the client added its own {ambient} header"
        );
    }
}

/// The credential the provider request carries.
const TOKEN: &str = "Bearer token-sentinel-6b0f";

/// The version header the provider request carries.
const PROVIDER_VERSION: &str = "2026-10-01";

/// The target the provider request names, before its fragment.
const PROVIDER_TARGET: &str = "/v1/answers?page=2";

/// The response ceiling the provider request sets, above its answer.
const PROVIDER_LIMIT: usize = 1024;

/// The exact bytes the provider request's payload serializes to.
const PROMPT_JSON: &str = r#"{"model":"provider-large","input":"summarize","attempts":3}"#;

/// The provider's answer.
const ANSWER_JSON: &[u8] = br#"{"id":"answer-1","accepted":true}"#;

/// Every header the provider request may carry, sorted. The transport's own
/// `accept` is removed before the comparison.
const PROVIDER_HEADERS: [&str; 5] = [
    "authorization",
    "content-length",
    "content-type",
    "host",
    "x-provider-version",
];

#[derive(Serialize)]
struct Prompt<'a> {
    model: &'a str,
    input: &'a str,
    attempts: u32,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct ProviderAnswer {
    id: Box<str>,
    accepted: bool,
}

/// Send the provider request the general builder exists for, and decode its
/// answer.
async fn ask_provider(
    client: &Client,
    url: &str,
    prompt: &Prompt<'_>,
) -> Result<(u16, ProviderAnswer), RuntimeError> {
    let request: ClientRequest<'_> = client
        .request("POST", url)?
        .sensitive_header("authorization", TOKEN)?
        .header("x-provider-version", PROVIDER_VERSION)?
        .json(prompt)?
        .response_limit(PROVIDER_LIMIT)?;
    let response: ClientResponse = request.send().await?.error_for_status()?;
    Ok((response.status(), response.json::<ProviderAnswer>()?))
}

/// An authenticated JSON request reaches the peer with its exact method,
/// target, headers, and body, and its answer decodes.
async fn authenticated_json_reaches_the_peer_exactly() {
    let row = "authenticated provider request";
    let reply = answer(
        "200 OK",
        &[("Content-Type", "application/json")],
        ANSWER_JSON,
    );
    let upstream = RecordingUpstream::bind(reply).await;
    let client: Client = http::client();
    let prompt = Prompt {
        model: "provider-large",
        input: "summarize",
        attempts: 3,
    };
    let url = format!("{}#fragment-is-local", upstream.url(PROVIDER_TARGET));

    let asked = within_bound(row, ask_provider(&client, &url, &prompt)).await;
    let recording = upstream.finish(row).await;

    let (status, decoded) = answered(row, asked);
    assert_eq!(status, 200, "{row}: the answer changed its status");
    let expected = ProviderAnswer {
        id: "answer-1".into(),
        accepted: true,
    };
    assert_eq!(decoded, expected, "{row}: the answer decoded differently");
    let [sent] = &*recording.requests else {
        panic!("{row}: the peer read {:?}", recording.requests);
    };
    let exact = Sent {
        method: "POST",
        target: PROVIDER_TARGET,
        content_type: Some("application/json"),
        body: PROMPT_JSON.as_bytes(),
    };
    assert_sent(row, sent, &exact);
    assert_provider_headers(row, sent);
}

/// Require the provider request's headers to be exactly the ones it set and
/// the ones its body and target require.
fn assert_provider_headers(row: &str, sent: &Recorded) {
    assert_eq!(
        *sent.header_values("authorization"),
        [TOKEN],
        "{row}: the credential changed on the wire"
    );
    assert_eq!(
        *sent.header_values("x-provider-version"),
        [PROVIDER_VERSION],
        "{row}: the application header changed on the wire"
    );
    assert_eq!(
        *sent.header_values("content-length"),
        [PROMPT_JSON.len().to_string().as_str()],
        "{row}: the declared length is not the payload's"
    );
    let mut names = sent.header_names().into_vec();
    names.retain(|name| name != "accept");
    names.sort();
    assert_eq!(
        names, PROVIDER_HEADERS,
        "{row}: the request carried other headers"
    );
}

/// The body every convenience that takes one sends.
const CONVENIENCE_BODY: &str = r#"{"convenience":"payload"}"#;

/// One verb convenience: its function name, its method, and the content type
/// it declares when it takes a body.
struct Convenience {
    name: &'static str,
    method: &'static str,
    content_type: Option<&'static str>,
}

const FORM: Option<&str> = Some("application/x-www-form-urlencoded");
const JSON: Option<&str> = Some("application/json");
const TEXT: Option<&str> = Some("text/plain");

/// Every verb convenience, free and reusable alike.
const CONVENIENCES: [Convenience; 14] = [
    convenience("get", "GET", None),
    convenience("post", "POST", TEXT),
    convenience("post_json", "POST", JSON),
    convenience("post_form", "POST", FORM),
    convenience("put", "PUT", TEXT),
    convenience("put_json", "PUT", JSON),
    convenience("put_form", "PUT", FORM),
    convenience("delete", "DELETE", None),
    convenience("delete_with_body", "DELETE", TEXT),
    convenience("patch", "PATCH", TEXT),
    convenience("patch_json", "PATCH", JSON),
    convenience("patch_form", "PATCH", FORM),
    convenience("head", "HEAD", None),
    convenience("options", "OPTIONS", None),
];

const fn convenience(
    name: &'static str,
    method: &'static str,
    content_type: Option<&'static str>,
) -> Convenience {
    Convenience {
        name,
        method,
        content_type,
    }
}

/// Call the convenience named `$name` on `$sender`, which is either the
/// `http::` path or a client followed by a dot.
macro_rules! call_convenience {
    ($name:expr, $url:expr, $($sender:tt)+) => {
        match $name {
            "get" => $($sender)+ get($url).await,
            "post" => $($sender)+ post($url, CONVENIENCE_BODY).await,
            "post_json" => $($sender)+ post_json($url, CONVENIENCE_BODY).await,
            "post_form" => $($sender)+ post_form($url, CONVENIENCE_BODY).await,
            "put" => $($sender)+ put($url, CONVENIENCE_BODY).await,
            "put_json" => $($sender)+ put_json($url, CONVENIENCE_BODY).await,
            "put_form" => $($sender)+ put_form($url, CONVENIENCE_BODY).await,
            "delete" => $($sender)+ delete($url).await,
            "delete_with_body" => $($sender)+ delete_with_body($url, CONVENIENCE_BODY).await,
            "patch" => $($sender)+ patch($url, CONVENIENCE_BODY).await,
            "patch_json" => $($sender)+ patch_json($url, CONVENIENCE_BODY).await,
            "patch_form" => $($sender)+ patch_form($url, CONVENIENCE_BODY).await,
            "head" => $($sender)+ head($url).await,
            "options" => $($sender)+ options($url).await,
            other => panic!("{other} is not a verb convenience"),
        }
    };
}

/// Call one free verb convenience.
async fn send_free(name: &str, url: &str) -> Result<ClientResponse, RuntimeError> {
    call_convenience!(name, url, http::)
}

/// Call one verb convenience of the reusable client.
async fn send_reusable(
    client: &Client,
    name: &str,
    url: &str,
) -> Result<ClientResponse, RuntimeError> {
    call_convenience!(name, url, client.)
}

/// The target one convenience row requests.
fn convenience_target(owner: &str, name: &str) -> String {
    format!("/convenience/{owner}/{name}")
}

/// Every free and reusable convenience sends one request with its own method,
/// content type, and body, and returns the typed response.
async fn every_convenience_sends_one_described_request() {
    let row = "verb conveniences";
    let upstream = RecordingUpstream::bind(empty_answer()).await;
    let client: Client = http::client();
    let mut statuses = Vec::new();

    for Convenience { name, .. } in &CONVENIENCES {
        let free_url = upstream.url(&convenience_target("free", name));
        let free: Result<ClientResponse, RuntimeError> =
            within_bound(row, send_free(name, &free_url)).await;
        let reusable_url = upstream.url(&convenience_target("reusable", name));
        let reusable: Result<ClientResponse, RuntimeError> =
            within_bound(row, send_reusable(&client, name, &reusable_url)).await;
        statuses.push((format!("the free {name}").into_boxed_str(), status_of(free)));
        statuses.push((
            format!("the reusable {name}").into_boxed_str(),
            status_of(reusable),
        ));
    }
    let recording = upstream.finish(row).await;

    assert_all_ok(row, &statuses);
    assert_eq!(
        recording.requests.len(),
        2 * CONVENIENCES.len(),
        "{row}: the peer read {:?}",
        recording.requests
    );
    for convenience in &CONVENIENCES {
        assert_convenience_sent(&recording, "free", convenience);
        assert_convenience_sent(&recording, "reusable", convenience);
    }
}

/// Require the one request a convenience sent to be the one its name states.
fn assert_convenience_sent(recording: &Recording, owner: &str, convenience: &Convenience) {
    let row = format!("{owner} {}", convenience.name);
    let target = convenience_target(owner, convenience.name);
    let body: &[u8] = match convenience.content_type {
        Some(_) => CONVENIENCE_BODY.as_bytes(),
        None => b"",
    };
    let expected = Sent {
        method: convenience.method,
        target: &target,
        content_type: convenience.content_type,
        body,
    };
    assert_sent(&row, recording.only_sent_to(&row, &target), &expected);
}

/// Extension tokens, and a standard name in another case. Each is its own
/// method.
const METHOD_TOKENS: [&str; 4] = ["PURGE", "get", "M-SEARCH", "Query_2"];

/// A method token is sent byte for byte: an extension method is not refused,
/// and a lowercase name is not folded into the standard one.
async fn method_tokens_are_sent_verbatim() {
    let row = "method tokens";
    let upstream = RecordingUpstream::bind(empty_answer()).await;
    let client: Client = http::client();
    let url = upstream.url("/method-token");
    let mut statuses = Vec::new();

    for method in METHOD_TOKENS {
        let draft = Draft { method, ..PLAIN };
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(row, submit(&client, &url, draft)).await;
        statuses.push((format!("{method:?}").into_boxed_str(), status_of(result)));
    }
    let recording = upstream.finish(row).await;

    assert_all_ok(row, &statuses);
    let sent: Box<[&str]> = recording.requests.iter().map(Recorded::method).collect();
    assert_eq!(
        *sent, METHOD_TOKENS,
        "{row}: the peer read other method tokens"
    );
}

/// The raw body the last-write rows set.
const RAW_BODY: &str = "raw-body-written-last";

/// The exact bytes the last-write rows' document serializes to.
const WRITTEN_JSON: &str = r#"{"written":"last"}"#;

/// One order of body and content-type writes.
#[derive(Clone, Copy, Debug)]
enum Write {
    JsonThenBody,
    BodyThenJson,
    ContentTypeThenJson,
    JsonThenContentType,
    BodyThenBody,
}

/// One last-write row: the order written, and what the peer must read.
struct WriteRow {
    write: Write,
    target: &'static str,
    content_type: Option<&'static str>,
    body: &'static str,
}

const WRITE_ROWS: [WriteRow; 5] = [
    // `body` replaces the bytes and leaves the content type `json` set.
    WriteRow {
        write: Write::JsonThenBody,
        target: "/write/json-then-body",
        content_type: JSON,
        body: RAW_BODY,
    },
    WriteRow {
        write: Write::BodyThenJson,
        target: "/write/body-then-json",
        content_type: JSON,
        body: WRITTEN_JSON,
    },
    // `json` replaces an earlier content type; it does not add a second one.
    WriteRow {
        write: Write::ContentTypeThenJson,
        target: "/write/content-type-then-json",
        content_type: JSON,
        body: WRITTEN_JSON,
    },
    WriteRow {
        write: Write::JsonThenContentType,
        target: "/write/json-then-content-type",
        content_type: Some("application/vnd.api+json"),
        body: WRITTEN_JSON,
    },
    // `body` alone invents no content type.
    WriteRow {
        write: Write::BodyThenBody,
        target: "/write/body-then-body",
        content_type: None,
        body: RAW_BODY,
    },
];

/// Send one POST whose body and content type were written in `write` order.
async fn send_written(
    client: &Client,
    url: &str,
    write: Write,
) -> Result<ClientResponse, RuntimeError> {
    let document = serde_json::json!({ "written": "last" });
    let request: ClientRequest<'_> = client.request("POST", url)?;
    let request: ClientRequest<'_> = match write {
        Write::JsonThenBody => request.json(&document)?.body(RAW_BODY),
        Write::BodyThenJson => request.body(RAW_BODY).json(&document)?,
        Write::ContentTypeThenJson => request
            .header("content-type", "application/x-ndjson")?
            .json(&document)?,
        Write::JsonThenContentType => request
            .json(&document)?
            .header("Content-Type", "application/vnd.api+json")?,
        Write::BodyThenBody => request.body("first-body").body(RAW_BODY),
    };
    request.send().await
}

/// The target of the header-replacement request.
const REPLACED_TARGET: &str = "/write/header-replacement";

/// A name written twice in different cases, once as sensitive and once not.
const REPLACED: Draft<'static> = Draft {
    sensitive: &[("x-api-key", "first-key")],
    headers: &[
        ("X-Api-Key", "second-key"),
        ("x-mode", "one"),
        ("X-MODE", "two"),
    ],
    ..PLAIN
};

/// The last write to the body, to the content type, and to a header name is
/// the one sent.
async fn last_write_decides_body_content_type_and_headers() {
    let row = "last write";
    let upstream = RecordingUpstream::bind(empty_answer()).await;
    let client: Client = http::client();
    let mut statuses = Vec::new();

    for WriteRow { write, target, .. } in &WRITE_ROWS {
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(row, send_written(&client, &upstream.url(target), *write)).await;
        statuses.push((*target, status_of(result)));
    }
    let replaced: Result<ClientResponse, RuntimeError> = within_bound(
        row,
        submit(&client, &upstream.url(REPLACED_TARGET), REPLACED),
    )
    .await;
    statuses.push((REPLACED_TARGET, status_of(replaced)));
    let recording = upstream.finish(row).await;

    assert_all_ok(row, &statuses);
    for written in &WRITE_ROWS {
        let expected = Sent {
            method: "POST",
            target: written.target,
            content_type: written.content_type,
            body: written.body.as_bytes(),
        };
        let context = format!("{row}, {:?}", written.write);
        assert_sent(
            &context,
            recording.only_sent_to(&context, written.target),
            &expected,
        );
    }
    let sent = recording.only_sent_to(row, REPLACED_TARGET);
    assert_eq!(
        *sent.header_values("x-api-key"),
        ["second-key"],
        "{row}: a replaced sensitive header must be sent once, with its last value"
    );
    assert_eq!(
        *sent.header_values("x-mode"),
        ["two"],
        "{row}: a header name must be replaced without regard to case"
    );
}

/// Where the redirect answer points. Relative, so it names the same peer.
const REDIRECT_LOCATION: &str = "/redirect/followed";

/// The body the redirect answer carries.
const REDIRECT_BODY: &[u8] = b"redirect";

/// By default a redirect is the call's answer: its status, its `Location`,
/// and its body are returned, and nothing is sent to the place it names.
async fn default_redirect_is_returned_without_a_second_send() {
    let row = "default redirect";
    let reply = answer(
        "302 Found",
        &[("Location", REDIRECT_LOCATION)],
        REDIRECT_BODY,
    );
    let upstream = RecordingUpstream::bind(reply).await;
    let client: Client = http::client();
    let targets = ["/redirect/described", "/redirect/free"];

    let described: Result<ClientResponse, RuntimeError> =
        within_bound(row, submit(&client, &upstream.url(targets[0]), PLAIN)).await;
    let free: Result<ClientResponse, RuntimeError> =
        within_bound(row, http::get(&upstream.url(targets[1]))).await;
    let recording = upstream.finish(row).await;

    for (ingress, result) in [("described", described), ("free", free)] {
        let response = answered(row, result);
        assert_eq!(
            response.status(),
            302,
            "{row}: the {ingress} call did not return the redirect"
        );
        assert_eq!(
            response.header("location"),
            Some(REDIRECT_LOCATION.as_bytes()),
            "{row}: the {ingress} call lost the Location"
        );
        assert_eq!(
            response.body_bytes(),
            REDIRECT_BODY,
            "{row}: the {ingress} call lost the redirect body"
        );
    }
    let sent: Box<[&str]> = recording.requests.iter().map(Recorded::target).collect();
    assert_eq!(*sent, targets, "{row}: a redirect caused another send");
}

/// 3.T1
#[tokio::test]
async fn provider_request_sends_exact_authenticated_json() {
    authenticated_json_reaches_the_peer_exactly().await;
    every_convenience_sends_one_described_request().await;
    method_tokens_are_sent_verbatim().await;
    last_write_decides_body_content_type_and_headers().await;
    default_redirect_is_returned_without_a_second_send().await;
}

/// The path segment every validation row's target carries.
const PATH_SECRET: &str = "path-sentinel-a41c";

/// The query value every validation row's target carries.
const QUERY_SECRET: &str = "query-sentinel-73de";

/// The fragment every validation row's target carries.
const FRAGMENT_SECRET: &str = "fragment-sentinel-0b6f";

/// The user name and password of the userinfo rows.
const USER_SECRET: &str = "user-sentinel-58aa";
const PASSWORD_SECRET: &str = "password-sentinel-c2e9";

/// The header value of the validation rows.
const HEADER_SECRET: &str = "header-sentinel-9d17";

/// The body of the validation rows.
const BODY_SECRET: &str = "body-sentinel-e350";

/// The text the failing serializer puts in its own error.
const ENCODE_SECRET: &str = "encode-sentinel-4f82";

/// A header value that carries a second header line behind a CR LF.
const INJECTED_VALUE: &str = "header-sentinel-9d17\r\nx-injected: 1";

/// How one validation row's target is written.
#[derive(Clone, Copy, Debug)]
enum Target {
    /// An absolute HTTP URL at the peer.
    Peer,
    /// A path with no scheme or host.
    Relative,
    /// An absolute URL at the peer under this scheme.
    Scheme(&'static str),
    /// A user name and a password before the host.
    Userinfo,
    /// A user name alone before the host.
    Username,
    /// At signs in the path and the query, and none in the authority.
    AtSign,
    /// An absolute HTTP URL at the peer with these control characters inside
    /// its path. A URL parser removes a tab and a line break.
    Control(&'static str),
}

/// The path, query, and fragment a secret-bearing target ends with.
fn secret_tail() -> String {
    format!("{PATH_SECRET}?token={QUERY_SECRET}#{FRAGMENT_SECRET}")
}

/// The URL one validation row requests.
fn target_url(target: Target, addr: SocketAddr) -> String {
    let tail = secret_tail();
    match target {
        Target::Peer => format!("http://{addr}/{tail}"),
        Target::Relative => format!("/{tail}"),
        Target::Scheme(scheme) => format!("{scheme}://{addr}/{tail}"),
        Target::Userinfo => format!("http://{USER_SECRET}:{PASSWORD_SECRET}@{addr}/{tail}"),
        Target::Username => format!("http://{USER_SECRET}@{addr}/{tail}"),
        Target::AtSign => format!("http://{addr}/users/a@b?mail=c@d"),
        Target::Control(control) => format!("http://{addr}/control{control}/{tail}"),
    }
}

/// The request target a peer reads for an admitted row: no fragment.
fn wire_target(target: Target) -> String {
    match target {
        Target::AtSign => "/users/a@b?mail=c@d".to_owned(),
        _ => format!("/{PATH_SECRET}?token={QUERY_SECRET}"),
    }
}

/// Everything a validation row's request carried that an error must not
/// repeat. The peer's address is here with the sentinels: an error that names
/// where it would have dialed has kept part of the URL.
fn secrets(addr: SocketAddr) -> [String; 9] {
    [
        PATH_SECRET.to_owned(),
        QUERY_SECRET.to_owned(),
        FRAGMENT_SECRET.to_owned(),
        USER_SECRET.to_owned(),
        PASSWORD_SECRET.to_owned(),
        HEADER_SECRET.to_owned(),
        BODY_SECRET.to_owned(),
        ENCODE_SECRET.to_owned(),
        addr.to_string(),
    ]
}

/// One request description that must be refused, and the category it is
/// refused under.
struct Refused<'d> {
    name: &'d str,
    target: Target,
    draft: Draft<'d>,
    kind: ClientInput,
}

const fn bad_method(name: &'static str, method: &'static str) -> Refused<'static> {
    Refused {
        name,
        target: Target::Peer,
        draft: Draft { method, ..PLAIN },
        kind: ClientInput::Method,
    }
}

const fn bad_url(name: &'static str, target: Target) -> Refused<'static> {
    Refused {
        name,
        target,
        draft: PLAIN,
        kind: ClientInput::Url,
    }
}

const fn bad_header(
    name: &'static str,
    headers: &'static [(&'static str, &'static str)],
) -> Refused<'static> {
    Refused {
        name,
        target: Target::Peer,
        draft: Draft { headers, ..PLAIN },
        kind: ClientInput::Header,
    }
}

const REFUSED: [Refused<'static>; 22] = [
    bad_method("empty method", ""),
    bad_method("method with a space", "GE T"),
    bad_method("method with a separator", "GET/1"),
    bad_method("method with a line break", "GET\r\nX-Injected: 1"),
    // A buffered response is not a tunnel.
    bad_method("CONNECT", "CONNECT"),
    bad_url("relative target", Target::Relative),
    bad_url("ftp target", Target::Scheme("ftp")),
    bad_url("websocket target", Target::Scheme("ws")),
    bad_url("target with userinfo", Target::Userinfo),
    bad_url("target with a user name", Target::Username),
    bad_url("target with a tab", Target::Control("\t")),
    bad_url("target with CR LF", Target::Control("\r\n")),
    bad_header("empty header name", &[("", HEADER_SECRET)]),
    bad_header("header name with a space", &[("x bad", HEADER_SECRET)]),
    bad_header("header name with a colon", &[("x-bad:", HEADER_SECRET)]),
    bad_header(
        "header name with a line break",
        &[("x-bad\r\nx-injected", HEADER_SECRET)],
    ),
    bad_header("header value with CR LF", &[("x-note", INJECTED_VALUE)]),
    bad_header("header value with LF", &[("x-note", "header\nsentinel")]),
    bad_header("header value with CR", &[("x-note", "header\rsentinel")]),
    Refused {
        name: "sensitive header value with CR LF",
        target: Target::Peer,
        draft: Draft {
            sensitive: &[("x-api-key", INJECTED_VALUE)],
            ..PLAIN
        },
        kind: ClientInput::Header,
    },
    Refused {
        name: "zero response limit",
        target: Target::Peer,
        draft: Draft {
            limit: Some(0),
            ..PLAIN
        },
        kind: ClientInput::ResponseLimit,
    },
    Refused {
        name: "TRACE with a body",
        target: Target::Peer,
        draft: Draft {
            method: "TRACE",
            body: Some(BODY_SECRET.as_bytes()),
            ..PLAIN
        },
        kind: ClientInput::Body,
    },
];

/// The headers the transport owns, each in the case a caller might write.
const RESERVED_HEADERS: [&str; 8] = [
    "Host",
    "Content-Length",
    "Transfer-Encoding",
    "Connection",
    "Upgrade",
    "TE",
    "Trailer",
    "Proxy-Authorization",
];

/// What one refused description returned.
struct Outcome {
    row: Box<str>,
    kind: ClientInput,
    result: Result<ClientResponse, RuntimeError>,
}

/// Build and send one description that must be refused, and keep its result.
async fn attempt(client: &Client, addr: SocketAddr, refused: &Refused<'_>) -> Outcome {
    let url = target_url(refused.target, addr);
    let result: Result<ClientResponse, RuntimeError> =
        within_bound(refused.name, submit(client, &url, refused.draft)).await;
    Outcome {
        row: refused.name.into(),
        kind: refused.kind,
        result,
    }
}

/// Attempt every reserved header in lowercase and in uppercase, through the
/// ordinary setter and through the sensitive one.
async fn attempt_reserved_headers(client: &Client, addr: SocketAddr) -> Vec<Outcome> {
    let spellings = RESERVED_HEADERS
        .iter()
        .flat_map(|name| [name.to_ascii_lowercase(), name.to_ascii_uppercase()]);
    let mut outcomes = Vec::new();
    for spelling in spellings {
        let entry = [(spelling.as_str(), HEADER_SECRET)];
        let ordinary = Refused {
            name: &format!("reserved header {spelling}"),
            target: Target::Peer,
            draft: Draft {
                headers: &entry,
                ..PLAIN
            },
            kind: ClientInput::ReservedHeader,
        };
        let sensitive = Refused {
            name: &format!("reserved sensitive header {spelling}"),
            target: Target::Peer,
            draft: Draft {
                sensitive: &entry,
                ..PLAIN
            },
            kind: ClientInput::ReservedHeader,
        };
        outcomes.push(attempt(client, addr, &ordinary).await);
        outcomes.push(attempt(client, addr, &sensitive).await);
    }
    outcomes
}

/// The targets every verb convenience must refuse, as the general request
/// refuses them.
const CONVENIENCE_REFUSALS: [(&str, Target); 5] = [
    ("relative target", Target::Relative),
    ("ftp target", Target::Scheme("ftp")),
    ("target with userinfo", Target::Userinfo),
    ("target with a tab", Target::Control("\t")),
    ("target with CR LF", Target::Control("\r\n")),
];

/// Call one verb convenience, free and reusable, with a target it must
/// refuse.
async fn attempt_convenience(
    client: &Client,
    addr: SocketAddr,
    name: &str,
    (case, target): (&str, Target),
) -> [Outcome; 2] {
    let url = target_url(target, addr);
    let free_row = format!("free {name}, {case}");
    let free: Result<ClientResponse, RuntimeError> =
        within_bound(&free_row, send_free(name, &url)).await;
    let reusable_row = format!("reusable {name}, {case}");
    let reusable: Result<ClientResponse, RuntimeError> =
        within_bound(&reusable_row, send_reusable(client, name, &url)).await;
    [(free_row, free), (reusable_row, reusable)].map(|(row, result)| Outcome {
        row: row.into_boxed_str(),
        kind: ClientInput::Url,
        result,
    })
}

/// Attempt every verb convenience, free and reusable, with each target it
/// must refuse. A convenience takes no method and no header from its caller,
/// so the target is the one part of it a caller can write wrong.
async fn attempt_conveniences(client: &Client, addr: SocketAddr) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for Convenience { name, .. } in &CONVENIENCES {
        for refusal in CONVENIENCE_REFUSALS {
            outcomes.extend(attempt_convenience(client, addr, name, refusal).await);
        }
    }
    outcomes
}

/// Require one outcome to be its refusal category, with nothing of the
/// request in any rendering of it.
fn assert_refused(outcome: Outcome, addr: SocketAddr) {
    let Outcome { row, kind, result } = outcome;
    let error = failure(&row, result);
    assert!(
        matches!(
            error,
            RuntimeError::Client(ClientError::InvalidRequest(found)) if found == kind
        ),
        "{row}: returned {error:?} instead of the {kind:?} refusal"
    );
    assert_safe(&error, &secrets(addr), &row);
}

/// A value whose serializer always fails, with a sentinel in its error.
struct Unserializable;

impl Serialize for Unserializable {
    fn serialize<S: Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom(ENCODE_SECRET))
    }
}

/// What `json` returned for a value that cannot be serialized: its failure,
/// or `None` if it built a request.
fn unserializable_json_failure(client: &Client, addr: SocketAddr) -> Option<RuntimeError> {
    let url = target_url(Target::Peer, addr);
    let described: Result<ClientRequest<'_>, RuntimeError> = client
        .request("POST", &url)
        .and_then(|request: ClientRequest<'_>| request.json(&Unserializable));
    described.err()
}

/// The header value of the redaction row that is not marked sensitive.
const NOTE_SECRET: &str = "note-sentinel-1ac4";

/// A request that carries a sentinel in every part a rendering could repeat.
const SECRET_BEARING: Draft<'static> = Draft {
    method: "POST",
    sensitive: &[("authorization", HEADER_SECRET)],
    headers: &[("x-note", NOTE_SECRET)],
    body: Some(BODY_SECRET.as_bytes()),
    limit: Some(64),
};

/// What a `Debug` rendering of a request or its client must not repeat: every
/// header value, marked or not, the body, and each secret-bearing part of the
/// URL.
const DESCRIPTION_SECRETS: [&str; 6] = [
    PATH_SECRET,
    QUERY_SECRET,
    FRAGMENT_SECRET,
    HEADER_SECRET,
    NOTE_SECRET,
    BODY_SECRET,
];

/// The `Debug` renderings of a secret-bearing request and of its client.
fn description_renderings(client: &Client, addr: SocketAddr) -> [String; 4] {
    let url = target_url(Target::Peer, addr);
    let request: ClientRequest<'_> = describe(client, &url, SECRET_BEARING)
        .unwrap_or_else(|error| panic!("request debug: the description failed: {error:?}"));
    [
        format!("{request:?}"),
        format!("{request:#?}"),
        format!("{client:?}"),
        format!("{client:#?}"),
    ]
}

/// The names the sensitivity row reads back, in the order it reads them.
const SENSITIVITY_NAMES: [&str; 5] = [
    "x-api-key",
    "authorization",
    "cookie",
    "x-plain",
    "x-absent",
];

/// A marked name later replaced by the ordinary setter, the two names that are
/// always sensitive written through the ordinary setter, and one plain name.
const SENSITIVITY: Draft<'static> = Draft {
    sensitive: &[("x-api-key", HEADER_SECRET)],
    headers: &[
        ("X-Api-Key", "replaced-by-the-ordinary-setter"),
        ("Authorization", HEADER_SECRET),
        ("Cookie", HEADER_SECRET),
        ("x-plain", "plain"),
    ],
    ..PLAIN
};

/// Whether each of [`SENSITIVITY_NAMES`] is marked sensitive on the request
/// the client would send.
///
/// `client_header_sensitivity(&request, name)` is the read-only view of the
/// production header projection: `Some(true)` when the stored header value
/// carries the sensitivity bit, `Some(false)` when it does not, and `None`
/// when the request holds no such header. It reads the request and changes
/// nothing.
fn observed_sensitivity(client: &Client, addr: SocketAddr) -> [Option<bool>; 5] {
    let url = target_url(Target::Peer, addr);
    let request: ClientRequest<'_> = describe(client, &url, SENSITIVITY)
        .unwrap_or_else(|error| panic!("header sensitivity: the description failed: {error:?}"));
    SENSITIVITY_NAMES.map(|name| client_header_sensitivity(&request, name))
}

/// One description beside a refused one that must be sent.
struct Admitted {
    name: &'static str,
    target: Target,
    draft: Draft<'static>,
}

const fn near_miss(name: &'static str, draft: Draft<'static>) -> Admitted {
    Admitted {
        name,
        target: Target::Peer,
        draft,
    }
}

const ADMITTED: [Admitted; 7] = [
    near_miss(
        "extension method beside CONNECT",
        Draft {
            method: "CONNECTED",
            ..PLAIN
        },
    ),
    near_miss(
        "TRACE without a body",
        Draft {
            method: "TRACE",
            ..PLAIN
        },
    ),
    near_miss(
        "POST with a body",
        Draft {
            method: "POST",
            body: Some(b"near-miss-body"),
            ..PLAIN
        },
    ),
    Admitted {
        name: "at signs outside the authority",
        target: Target::AtSign,
        draft: PLAIN,
    },
    near_miss(
        "names beside the reserved ones",
        Draft {
            headers: &[
                ("x-host", "peer"),
                ("content-language", "en"),
                ("x-connection", "kept"),
                ("trailers-seen", "1"),
            ],
            ..PLAIN
        },
    ),
    near_miss(
        "value with a tab and spaces",
        Draft {
            headers: &[("x-note", "kept\tvalue with spaces")],
            ..PLAIN
        },
    ),
    near_miss(
        "response limit of one byte",
        Draft {
            limit: Some(1),
            ..PLAIN
        },
    ),
];

/// Each valid near miss of a refused description is sent once, as written.
/// The rules refuse their cases and nothing wider.
async fn near_misses_are_sent() {
    let row = "valid near misses";
    let upstream = RecordingUpstream::bind(empty_answer()).await;
    let addr = upstream.addr();
    let client: Client = http::client();
    let mut statuses = Vec::new();

    for admitted in &ADMITTED {
        let url = target_url(admitted.target, addr);
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(admitted.name, submit(&client, &url, admitted.draft)).await;
        statuses.push((admitted.name, status_of(result)));
    }
    let recording = upstream.finish(row).await;

    assert_all_ok(row, &statuses);
    assert_eq!(
        recording.requests.len(),
        ADMITTED.len(),
        "{row}: the peer read {:?}",
        recording.requests
    );
    for (admitted, sent) in ADMITTED.iter().zip(recording.requests.iter()) {
        assert_near_miss_sent(admitted, sent);
    }
}

/// Require one near miss to have reached the peer as it was described.
fn assert_near_miss_sent(admitted: &Admitted, sent: &Recorded) {
    let name = admitted.name;
    let draft = admitted.draft;
    assert_eq!(sent.method(), draft.method, "{name}: the method changed");
    assert_eq!(
        sent.target(),
        wire_target(admitted.target),
        "{name}: the target changed"
    );
    assert_eq!(
        &*sent.body,
        draft.body.unwrap_or_default(),
        "{name}: the body changed"
    );
    for (header, value) in draft.headers {
        assert_eq!(
            *sent.header_values(header),
            [*value],
            "{name}: the header {header} changed"
        );
    }
}

/// 3.T2
#[tokio::test]
async fn request_validation_prevents_network_and_secret_projection() {
    let row = "request validation";
    let upstream = RecordingUpstream::bind(empty_answer()).await;
    let addr = upstream.addr();
    let client: Client = http::client();

    let mut outcomes = Vec::new();
    for refused in &REFUSED {
        outcomes.push(attempt(&client, addr, refused).await);
    }
    outcomes.extend(attempt_reserved_headers(&client, addr).await);
    outcomes.extend(attempt_conveniences(&client, addr).await);
    let encode_failure = unserializable_json_failure(&client, addr);
    let renderings = description_renderings(&client, addr);
    let sensitivity = observed_sensitivity(&client, addr);
    let recording = upstream.finish(row).await;

    for outcome in outcomes {
        assert_refused(outcome, addr);
    }
    let Some(encode_error) = encode_failure else {
        panic!("{row}: json built a request from a value that cannot be serialized");
    };
    assert!(
        matches!(encode_error, RuntimeError::Client(ClientError::Encode)),
        "{row}: json returned {encode_error:?} instead of the encode failure"
    );
    assert_safe(&encode_error, &secrets(addr), "unserializable json");
    for rendering in &renderings {
        assert!(!rendering.is_empty(), "{row}: one Debug rendering is empty");
        for secret in DESCRIPTION_SECRETS {
            assert!(
                !rendering.contains(secret),
                "{row}: the secret {secret:?} reached a Debug rendering: {rendering}"
            );
        }
    }
    assert_eq!(
        sensitivity,
        [Some(true), Some(true), Some(true), Some(false), None],
        "{row}: sensitivity of {SENSITIVITY_NAMES:?}"
    );
    assert_eq!(
        (
            recording.connections,
            recording.starts,
            recording.requests.len()
        ),
        (0, 0, 0),
        "{row}: a refused description reached the peer: {:?}",
        recording.requests
    );

    near_misses_are_sent().await;
}

/// The answer the isolation rows collect.
const ISOLATION_ANSWER: &[u8] = b"sixteen-byte-ans";

/// A request whose ceiling is one byte below the answer.
const NARROW: Draft<'static> = Draft {
    method: "POST",
    sensitive: &[("authorization", "Bearer narrow")],
    headers: &[("x-tenant", "narrow"), ("x-only-narrow", "1")],
    body: Some(b"narrow-body"),
    limit: Some(ISOLATION_ANSWER.len() - 1),
};

/// A request whose ceiling is exactly the answer.
const EXACT: Draft<'static> = Draft {
    method: "PUT",
    sensitive: &[("authorization", "Bearer exact")],
    headers: &[("x-tenant", "exact")],
    body: Some(b"exact-body"),
    limit: Some(ISOLATION_ANSWER.len()),
};

/// A request under the client's own ceiling.
const DEFAULTED: Draft<'static> = Draft {
    method: "POST",
    sensitive: &[],
    headers: &[("x-tenant", "defaulted")],
    body: Some(b"defaulted-body"),
    limit: None,
};

/// Every header name an isolation row writes. A request that carries one it
/// did not write took it from a sibling.
const ISOLATED_HEADERS: [&str; 3] = ["authorization", "x-tenant", "x-only-narrow"];

/// Require one isolated request to have carried its own description and
/// nothing of a sibling's.
fn assert_isolated(row: &str, recording: &Recording, target: &str, draft: Draft<'_>) {
    let sent = recording.only_sent_to(row, target);
    let expected = Sent {
        method: draft.method,
        target,
        content_type: None,
        body: draft.body.unwrap_or_default(),
    };
    assert_sent(row, sent, &expected);
    for name in ISOLATED_HEADERS {
        let written: Box<[&str]> = draft
            .sensitive
            .iter()
            .chain(draft.headers)
            .filter(|(header, _)| *header == name)
            .map(|(_, value)| *value)
            .collect();
        assert_eq!(
            sent.header_values(name),
            written,
            "{row}: {target} did not carry exactly its own {name}"
        );
    }
}

/// Concurrent requests through one client keep their own headers, bodies, and
/// ceilings. A ceiling that refuses one answer does not refuse a sibling's,
/// and it is gone from the client afterwards.
async fn concurrent_requests_keep_their_own_policy() {
    let row = "request isolation";
    let upstream = RecordingUpstream::bind(answer("200 OK", &[], ISOLATION_ANSWER)).await;
    let client: Client = http::client();
    let targets = [
        "/isolation/narrow",
        "/isolation/exact",
        "/isolation/defaulted",
        "/isolation/later",
    ];
    let urls = targets.map(|target| upstream.url(target));

    let (narrowed, exact, defaulted) = within_bound(row, async {
        tokio::join!(
            submit(&client, &urls[0], NARROW),
            submit(&client, &urls[1], EXACT),
            submit(&client, &urls[2], DEFAULTED),
        )
    })
    .await;
    let later: Result<ClientResponse, RuntimeError> = within_bound(row, client.get(&urls[3])).await;
    let recording = upstream.finish(row).await;

    assert_client_limit(row, ISOLATION_ANSWER.len() - 1, narrowed);
    for (name, result) in [("exact", exact), ("defaulted", defaulted), ("later", later)] {
        assert_eq!(
            answered(row, result).body_bytes(),
            ISOLATION_ANSWER,
            "{row}: the {name} request lost its answer to a sibling's ceiling"
        );
    }
    assert_eq!(
        recording.requests.len(),
        targets.len(),
        "{row}: the peer read {:?}",
        recording.requests
    );
    assert_isolated(row, &recording, targets[0], NARROW);
    assert_isolated(row, &recording, targets[1], EXACT);
    assert_isolated(row, &recording, targets[2], DEFAULTED);
    assert_isolated(row, &recording, targets[3], PLAIN);
}

/// How many described requests the reuse row sends before its convenience.
const REUSED_REQUESTS: usize = 3;

/// Successive requests through one client ride one retained connection. Each
/// answer is collected whole first, so the transport is free to be reused.
async fn repeated_requests_reuse_one_connection() {
    let row = "transport reuse";
    let upstream = RecordingUpstream::bind(answer("200 OK", &[], b"reused")).await;
    let client: Client = http::client();
    let draft = Draft {
        method: "POST",
        body: Some(b"reuse"),
        ..PLAIN
    };
    let mut statuses = Vec::new();

    for index in 0..REUSED_REQUESTS {
        let target = format!("/reuse/{index}");
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(row, submit(&client, &upstream.url(&target), draft)).await;
        statuses.push((target.into_boxed_str(), status_of(result)));
    }
    let last = "/reuse/convenience";
    let convenience: Result<ClientResponse, RuntimeError> =
        within_bound(row, client.get(&upstream.url(last))).await;
    statuses.push((last.into(), status_of(convenience)));
    let recording = upstream.finish(row).await;

    assert_all_ok(row, &statuses);
    let connections: Box<[usize]> = recording
        .requests
        .iter()
        .map(|request| request.connection)
        .collect();
    assert_eq!(
        *connections,
        [0; REUSED_REQUESTS + 1],
        "{row}: the requests did not share the first connection"
    );
    assert_eq!(
        recording.connections, 1,
        "{row}: the client dialed {} times",
        recording.connections
    );
}

/// The body the admission mock answers with.
const MOCK_BODY: &[u8] = b"mock-answer";

/// A mock answers only a request that passed validation, and only under the
/// request's own ceiling.
///
/// The two refused descriptions are attempted first. The mock is then called
/// once by a valid request, so a refused description that was answered would
/// make that count two.
async fn mock_answers_only_a_valid_request_under_its_ceiling() {
    let row = "mock admission";
    let url = "http://client-request.mock.test/admission";
    let canned = Response::bytes(200, MOCK_BODY).expect("a valid mock status");
    let registered = mock::http(url).returns(canned);
    let client: Client = http::client();
    let reserved = Draft {
        headers: &[("host", "elsewhere.test")],
        ..PLAIN
    };
    let traced = Draft {
        method: "TRACE",
        body: Some(b"trace-body"),
        ..PLAIN
    };
    let exact = Draft {
        limit: Some(MOCK_BODY.len()),
        ..PLAIN
    };
    let narrow = Draft {
        limit: Some(MOCK_BODY.len() - 1),
        ..PLAIN
    };

    let refused = [
        ("reserved header", ClientInput::ReservedHeader, reserved),
        ("TRACE with a body", ClientInput::Body, traced),
    ];
    let mut refusals = Vec::new();
    for (name, kind, draft) in refused {
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(row, submit(&client, url, draft)).await;
        refusals.push((name, kind, result));
    }
    let admitted: Result<ClientResponse, RuntimeError> =
        within_bound(row, submit(&client, url, exact)).await;
    registered.assert_called_once();
    let narrowed: Result<ClientResponse, RuntimeError> =
        within_bound(row, submit(&client, url, narrow)).await;
    drop(registered);

    for (name, kind, result) in refusals {
        assert!(
            matches!(
                result,
                Err(RuntimeError::Client(ClientError::InvalidRequest(found))) if found == kind
            ),
            "{row}: a request with a {name} returned {result:?}"
        );
    }
    assert_eq!(
        answered(row, admitted).body_bytes(),
        MOCK_BODY,
        "{row}: the mock answer at its ceiling changed"
    );
    assert_client_limit(row, MOCK_BODY.len() - 1, narrowed);
}

/// A GET registration answers GET and nothing else. An extension method, and
/// `get` in lowercase, go to the network.
async fn extension_method_never_matches_a_get_registration() {
    let row = "method-specific mock";
    let upstream = RecordingUpstream::bind(answer("200 OK", &[], b"peer")).await;
    let url = upstream.url("/mock/get-only");
    let canned = Response::text(200, "mock").expect("a valid mock status");
    let registered = mock::http_method(Method::Get, &url).returns(canned);
    let client: Client = http::client();
    let methods = ["PURGE", "get", "GET"];

    let mut results = Vec::new();
    for method in methods {
        let draft = Draft { method, ..PLAIN };
        let result: Result<ClientResponse, RuntimeError> =
            within_bound(row, submit(&client, &url, draft)).await;
        results.push(result);
    }
    registered.assert_called_once();
    drop(registered);
    let recording = upstream.finish(row).await;

    let bodies: Vec<Bytes> = results
        .into_iter()
        .map(|result| answered(row, result).into_body())
        .collect();
    assert_eq!(
        bodies,
        ["peer", "peer", "mock"],
        "{row}: answers to {methods:?}"
    );
    let sent: Box<[&str]> = recording.requests.iter().map(Recorded::method).collect();
    assert_eq!(
        *sent,
        ["PURGE", "get"],
        "{row}: the peer read other requests"
    );
}

/// A registration with no method answers an extension method.
async fn method_agnostic_mock_answers_an_extension_method() {
    let row = "method-agnostic mock";
    let url = "http://client-request.mock.test/any-method";
    let canned = Response::text(200, "any").expect("a valid mock status");
    let registered = mock::http(url).returns(canned);
    let client: Client = http::client();
    let draft = Draft {
        method: "PURGE",
        ..PLAIN
    };

    let result: Result<ClientResponse, RuntimeError> =
        within_bound(row, submit(&client, url, draft)).await;
    registered.assert_called_once();
    drop(registered);

    assert_eq!(
        answered(row, result).body_bytes(),
        b"any",
        "{row}: the mock did not answer the extension method"
    );
}

/// 3.T3
#[tokio::test]
async fn request_policy_isolation_and_mock_admission_are_local() {
    concurrent_requests_keep_their_own_policy().await;
    repeated_requests_reuse_one_connection().await;
    mock_answers_only_a_valid_request_under_its_ceiling().await;
    extension_method_never_matches_a_get_registration().await;
    method_agnostic_mock_answers_an_extension_method().await;
}
