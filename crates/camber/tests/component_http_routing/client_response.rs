//! 2.T1 and 2.T2: an outbound answer is a buffered `ClientResponse` that keeps
//! the bytes the peer sent and converts them only where the caller asks.
//!
//! Each row sends an existing public verb at a scripted raw peer that writes
//! one exact wire image. The answer is read through every accessor while the
//! peer still counts request starts. A local conversion that sent again would
//! be counted before the peer is joined. Assertions run after the join.

use crate::bounded_client::{ADMITTED, CEILING, assert_client_limit, bounded_client, within_bound};
use crate::error_chain::assert_secret_free;
use crate::retry_upstream::{
    Answer, CLIENT_METHODS, PeerEvent, RunnableDriver, ScriptedUpstream, answered, send_method,
};

use camber::RuntimeError;
use camber::http;
use camber::http::Bytes;
use camber::http::ClientDecode;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::Response;
use camber::http::mock;
use flate2::read::GzDecoder;
use std::future::Future;
use std::io::Read;

/// The one route every row requests.
const TARGET: &str = "/client-response";

/// What one answer showed through every accessor.
///
/// Owned, so a row asserts on it after its peer is stopped and joined.
#[derive(Debug)]
struct Observed {
    status: u16,
    /// Every header entry, in the order `headers` yields it.
    headers: Box<[(Box<str>, Box<[u8]>)]>,
    /// What `header` answered for each name a row looked up, in that order.
    first: Box<[Option<Box<[u8]>>]>,
    body: Box<[u8]>,
    text: Result<Box<str>, RuntimeError>,
    json: Result<serde_json::Value, RuntimeError>,
    /// The `Debug` and alternate `Debug` renderings.
    renderings: [String; 2],
    /// The owned body `error_for_status` then `into_body` hands over.
    converted: Result<Bytes, RuntimeError>,
}

impl Observed {
    /// Every value `headers` yields for `name`, in the order it yields them.
    fn values_of(&self, name: &str) -> Box<[&[u8]]> {
        self.headers
            .iter()
            .filter(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| &**value)
            .collect()
    }
}

/// Read one answer through every accessor, then consume it.
///
/// The borrowing accessors run first. `error_for_status` and `into_body` take
/// the response, so they run last.
fn observe(response: ClientResponse, lookups: &[&str]) -> Observed {
    let status = response.status();
    let headers = response
        .headers()
        .map(|(name, value)| (Box::from(name), Box::from(value)))
        .collect();
    let first = lookups
        .iter()
        .map(|name| response.header(name).map(Box::from))
        .collect();
    let body = Box::from(response.body_bytes());
    let text = response.text().map(Box::from);
    let json = response.json::<serde_json::Value>();
    let renderings = [format!("{response:?}"), format!("{response:#?}")];
    let converted = response.error_for_status().map(ClientResponse::into_body);
    Observed {
        status,
        headers,
        first,
        body,
        text,
        json,
        renderings,
        converted,
    }
}

/// Send one request at a peer that writes `wire`, and hand the call's result
/// to `inspect` while the peer still counts request starts.
///
/// The peer is then stopped and joined. Only after that does this require the
/// peer's evidence: one request read, one answer written, and no second start.
async fn exchange<F, T>(
    row: &str,
    wire: &'static [u8],
    send: impl FnOnce(String) -> F,
    inspect: impl FnOnce(Result<ClientResponse, RuntimeError>) -> T,
) -> T
where
    F: Future<Output = Result<ClientResponse, RuntimeError>>,
{
    let mut upstream = ScriptedUpstream::bind(&[Answer::Exact(wire)]).await;
    let fetched: Result<ClientResponse, RuntimeError> =
        within_bound(row, send(upstream.url(TARGET))).await;
    let started = upstream.saw(PeerEvent::Started(0)).await;
    let wrote = upstream.saw(PeerEvent::Answered(0)).await;
    let inspected = inspect(fetched);
    // The rows run on real time, so the driver has no clock to hold still.
    let starts = upstream.finish(RunnableDriver::start(), row).await;

    assert_eq!(started, Ok(()), "{row}: the peer never read the request");
    assert_eq!(wrote, Ok(()), "{row}: the peer never wrote its answer");
    assert_eq!(
        starts, 1,
        "{row}: the call and its local conversions started {starts} requests"
    );
    inspected
}

/// Send one request at a peer that writes `wire`, and return what the answer
/// showed.
async fn answer_to<F>(
    row: &str,
    wire: &'static [u8],
    lookups: &[&str],
    send: impl FnOnce(String) -> F,
) -> Observed
where
    F: Future<Output = Result<ClientResponse, RuntimeError>>,
{
    let observed = exchange(row, wire, send, |fetched| {
        fetched.map(|response| observe(response, lookups))
    })
    .await;
    answered(row, observed)
}

/// Send the free `http::get` at a peer that writes `wire`, and return what the
/// answer showed.
async fn answer_to_get(row: &str, wire: &'static [u8], lookups: &[&str]) -> Observed {
    answer_to(
        row,
        wire,
        lookups,
        |url| async move { http::get(&url).await },
    )
    .await
}

/// The bytes a wire image carries after its head.
fn payload(wire: &'static [u8]) -> &'static [u8] {
    let head_end = wire
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("a wire image has no end of head"));
    &wire[head_end + 4..]
}

/// Whether `result` is the decode failure `kind`.
fn is_decode<T>(result: &Result<T, RuntimeError>, kind: ClientDecode) -> bool {
    matches!(
        result,
        Err(RuntimeError::Client(ClientError::Decode(found))) if *found == kind
    )
}

/// Whether `result` is the explicit status refusal for `status`.
fn is_status<T>(result: &Result<T, RuntimeError>, status: u16) -> bool {
    matches!(
        result,
        Err(RuntimeError::Client(ClientError::Status(found))) if *found == status
    )
}

/// Require the owned body to be exactly the bytes the peer sent.
fn assert_owned_body(row: &str, observed: &Observed, sent: &[u8]) {
    match &observed.converted {
        Ok(owned) => assert_eq!(&owned[..], sent, "{row}: into_body handed over other bytes"),
        Err(error) => panic!("{row}: the status conversion refused the answer: {error:?}"),
    }
}

/// A header value that is not text in any encoding the client could assume.
const OPAQUE_VALUE: &[u8] = b"caf\xe9\xff";

/// One name sent twice around another name, and one value that is not UTF-8.
const HEADER_WIRE: &[u8] = b"HTTP/1.1 200 OK\r\n\
    Set-Cookie: first=1\r\n\
    X-Between: other\r\n\
    Set-Cookie: second=2\r\n\
    X-Opaque: caf\xe9\xff\r\n\
    Content-Length: 2\r\n\
    Connection: close\r\n\r\nok";

/// Repeated values keep their order, and a value that is not text keeps its
/// bytes.
async fn headers_keep_repeats_and_raw_bytes() {
    let row = "repeated and non-text headers";
    let lookups = ["set-cookie", "Set-Cookie", "x-opaque", "x-absent"];
    let observed = answer_to_get(row, HEADER_WIRE, &lookups).await;

    assert_eq!(
        *observed.values_of("set-cookie"),
        [&b"first=1"[..], &b"second=2"[..]],
        "{row}: a repeated name must keep every value in the order sent"
    );
    assert_eq!(
        *observed.values_of("x-opaque"),
        [OPAQUE_VALUE],
        "{row}: a non-text value must keep its bytes"
    );
    assert_eq!(
        *observed.first,
        [
            Some(Box::from(&b"first=1"[..])),
            Some(Box::from(&b"first=1"[..])),
            Some(Box::from(OPAQUE_VALUE)),
            None,
        ],
        "{row}: header must answer the first value for {lookups:?}"
    );
}

/// One body row: a wire image and the text its body decodes to, if any.
struct BodyRow {
    name: &'static str,
    wire: &'static [u8],
    text: Option<&'static str>,
}

const BODY_ROWS: [BodyRow; 3] = [
    BodyRow {
        name: "binary body",
        wire: b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\n\x00\xffbin\x80",
        text: None,
    },
    BodyRow {
        name: "utf-8 body",
        wire: b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncaf\xc3\xa9 ok",
        text: Some("caf\u{e9} ok"),
    },
    BodyRow {
        name: "empty body",
        wire: b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        text: Some(""),
    },
];

/// A body is kept byte for byte, and text decoding is strict.
async fn bodies_keep_their_bytes_and_text_is_strict() {
    for BodyRow { name, wire, text } in BODY_ROWS {
        let observed = answer_to_get(name, wire, &[]).await;
        let sent = payload(wire);

        assert_eq!(&*observed.body, sent, "{name}: body_bytes changed the body");
        assert_owned_body(name, &observed, sent);
        match text {
            Some(decoded) => assert_eq!(
                observed.text.as_deref().ok(),
                Some(decoded),
                "{name}: text returned {:?}",
                observed.text
            ),
            None => assert!(
                is_decode(&observed.text, ClientDecode::Utf8),
                "{name}: text returned {:?} for a body that is not UTF-8",
                observed.text
            ),
        }
    }
}

/// One JSON row: a wire image and whether its body is the expected document.
struct JsonRow {
    name: &'static str,
    wire: &'static [u8],
    valid: bool,
}

const JSON_ROWS: [JsonRow; 3] = [
    JsonRow {
        name: "valid JSON declared as text",
        wire: b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 27\r\n\
            Connection: close\r\n\r\n{\"name\":\"camber\",\"count\":3}",
        valid: true,
    },
    JsonRow {
        name: "valid JSON with no content type",
        wire: b"HTTP/1.1 200 OK\r\nContent-Length: 27\r\n\
            Connection: close\r\n\r\n{\"name\":\"camber\",\"count\":3}",
        valid: true,
    },
    JsonRow {
        name: "invalid JSON declared as JSON",
        wire: b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\
            Connection: close\r\n\r\n{\"name\":",
        valid: false,
    },
];

/// JSON decoding reads the body and never the declared content type.
async fn json_decoding_ignores_the_content_type() {
    let document = serde_json::json!({ "name": "camber", "count": 3 });
    for JsonRow { name, wire, valid } in JSON_ROWS {
        let observed = answer_to_get(name, wire, &[]).await;

        assert_eq!(
            &*observed.body,
            payload(wire),
            "{name}: decoding changed the retained body"
        );
        match valid {
            true => assert_eq!(
                observed.json.as_ref().ok(),
                Some(&document),
                "{name}: json returned {:?}",
                observed.json
            ),
            false => assert!(
                is_decode(&observed.json, ClientDecode::Json),
                "{name}: json returned {:?} for a body that is not JSON",
                observed.json
            ),
        }
    }
}

/// One status row: a wire image, its status, and whether the explicit
/// conversion refuses it.
struct StatusRow {
    name: &'static str,
    wire: &'static [u8],
    status: u16,
    refused: bool,
}

const STATUS_ROWS: [StatusRow; 4] = [
    StatusRow {
        name: "status 399",
        wire: b"HTTP/1.1 399 Below\r\nContent-Length: 2\r\nConnection: close\r\n\r\nst",
        status: 399,
        refused: false,
    },
    StatusRow {
        name: "status 400",
        wire: b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\nConnection: close\r\n\r\nst",
        status: 400,
        refused: true,
    },
    StatusRow {
        name: "status 599",
        wire: b"HTTP/1.1 599 Above\r\nContent-Length: 2\r\nConnection: close\r\n\r\nst",
        status: 599,
        refused: true,
    },
    StatusRow {
        name: "status 600",
        wire: b"HTTP/1.1 600 Beyond\r\nContent-Length: 2\r\nConnection: close\r\n\r\nst",
        status: 600,
        refused: false,
    },
];

/// The send returns every status as a response. Only the explicit conversion
/// refuses 400 through 599.
async fn status_conversion_is_explicit_at_each_boundary() {
    for row in STATUS_ROWS {
        let name = row.name;
        let observed = answer_to_get(name, row.wire, &[]).await;

        assert_eq!(
            observed.status, row.status,
            "{name}: the send changed the status"
        );
        assert_eq!(
            &*observed.body,
            payload(row.wire),
            "{name}: the body of an error status must be collected whole"
        );
        match row.refused {
            true => assert!(
                is_status(&observed.converted, row.status),
                "{name}: error_for_status returned {:?}",
                observed.converted
            ),
            false => assert_owned_body(name, &observed, payload(row.wire)),
        }
    }
}

/// The header value the redaction row's answer carries.
const HEADER_SECRET: &str = "header-sentinel-51c7";

/// The body the redaction row's answer carries.
const BODY_SECRET: &str = "body-sentinel-2e9d";

/// An error status whose header value and body are both sentinels.
const SECRET_WIRE: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\n\
    X-Secret: header-sentinel-51c7\r\n\
    Content-Length: 18\r\n\
    Connection: close\r\n\r\nbody-sentinel-2e9d";

/// `Debug` shows no header value and no body, and neither does the status
/// refusal built from the same answer.
async fn debug_renders_no_header_value_or_body() {
    let row = "debug redaction";
    let observed = answer_to_get(row, SECRET_WIRE, &["x-secret"]).await;

    assert_eq!(
        *observed.first,
        [Some(Box::from(HEADER_SECRET.as_bytes()))],
        "{row}: the accessor must still return the header value"
    );
    assert_eq!(
        &*observed.body,
        BODY_SECRET.as_bytes(),
        "{row}: the accessor must still return the body"
    );
    for rendering in &observed.renderings {
        assert!(!rendering.is_empty(), "{row}: one Debug rendering is empty");
        for secret in [HEADER_SECRET, BODY_SECRET] {
            assert!(
                !rendering.contains(secret),
                "{row}: the secret {secret:?} reached a Debug rendering: {rendering}"
            );
        }
    }
    let Err(refusal) = &observed.converted else {
        panic!("{row}: error_for_status admitted status 500");
    };
    assert!(
        is_status(&observed.converted, 500),
        "{row}: error_for_status returned {refusal:?}"
    );
    assert_secret_free(refusal, &[HEADER_SECRET, BODY_SECRET], row);
}

/// The gzip encoding of `hello`.
const GZIP_HELLO: &[u8] = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\
    \xcb\x48\xcd\xc9\xc9\x07\x00\x86\xa6\x10\x36\x05\x00\x00\x00";

/// An answer that declares gzip and carries [`GZIP_HELLO`].
const GZIP_WIRE: &[u8] = b"HTTP/1.1 200 OK\r\n\
    Content-Encoding: gzip\r\n\
    Content-Length: 25\r\n\
    Connection: close\r\n\r\n\
    \x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\
    \xcb\x48\xcd\xc9\xc9\x07\x00\x86\xa6\x10\x36\x05\x00\x00\x00";

/// An encoded answer is returned encoded.
///
/// The fixture is first shown to be real gzip, so a transport that decoded it
/// would hand back `hello`. With Reqwest's gzip feature unified into the
/// graph, only the client's explicit setting keeps these bytes.
async fn encoded_body_is_returned_encoded() {
    let row = "gzip answer";
    let mut decoded = Vec::new();
    GzDecoder::new(GZIP_HELLO)
        .read_to_end(&mut decoded)
        .expect("the fixture is a complete gzip member");
    assert_eq!(
        decoded, b"hello",
        "{row}: the fixture decodes to other bytes"
    );
    assert_eq!(
        payload(GZIP_WIRE),
        GZIP_HELLO,
        "{row}: the wire carries another body"
    );

    let lookups = ["content-encoding", "content-length"];
    let observed = answer_to_get(row, GZIP_WIRE, &lookups).await;

    assert_eq!(
        &*observed.body, GZIP_HELLO,
        "{row}: the client must not decode the body"
    );
    assert_owned_body(row, &observed, GZIP_HELLO);
    assert_eq!(
        *observed.first,
        [Some(Box::from(&b"gzip"[..])), Some(Box::from(&b"25"[..]))],
        "{row}: the encoding headers must describe the retained bytes"
    );
}

/// An empty answer every method may receive, HEAD included.
const VERB_WIRE: &[u8] = b"HTTP/1.1 200 OK\r\n\
    X-Verb: answered\r\n\
    Content-Length: 0\r\n\
    Connection: close\r\n\r\n";

/// Every method the reusable client offers returns the same typed response.
async fn every_reusable_verb_returns_the_typed_response() {
    for method in CLIENT_METHODS {
        let row = format!("reusable {method}");
        let observed = answer_to(&row, VERB_WIRE, &["x-verb"], |url| async move {
            send_method(&http::client(), method, &url).await
        })
        .await;

        assert_eq!(observed.status, 200, "{row}: unexpected status");
        assert_eq!(
            *observed.first,
            [Some(Box::from(&b"answered"[..]))],
            "{row}: the answer lost its header"
        );
        assert_owned_body(&row, &observed, b"");
    }
}

/// 2.T1
#[tokio::test]
async fn outbound_response_preserves_bytes_and_local_conversions() {
    headers_keep_repeats_and_raw_bytes().await;
    bodies_keep_their_bytes_and_text_is_strict().await;
    json_decoding_ignores_the_content_type().await;
    status_conversion_is_explicit_at_each_boundary().await;
    debug_renders_no_header_value_or_body().await;
    encoded_body_is_returned_encoded().await;
    every_reusable_verb_returns_the_typed_response().await;
}

/// A final error status whose body exactly fills [`CEILING`].
const ERROR_AT_CEILING_WIRE: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\n\
    Content-Length: 16\r\n\
    Connection: close\r\n\r\n0123456789abcdef";

/// A final error status that declares more than [`CEILING`]. The peer sends
/// none of it.
const ERROR_ABOVE_CEILING_WIRE: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 64\r\nConnection: close\r\n\r\n";

/// A final error status whose body ends short of its declared length.
const ERROR_TRUNCATED_WIRE: &[u8] =
    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 16\r\nConnection: close\r\n\r\npartial";

/// Send one bounded GET at a peer that writes `wire`, and return the call's
/// own result.
async fn bounded_result(row: &str, wire: &'static [u8]) -> Result<ClientResponse, RuntimeError> {
    exchange(
        row,
        wire,
        |url| async move { bounded_client().get(&url).await },
        |fetched| fetched,
    )
    .await
}

/// An error status at the ceiling is collected whole before it is returned,
/// and it stays a response until the caller converts it.
async fn final_error_status_at_the_ceiling_is_collected_whole() {
    let row = "error status at the ceiling";
    let observed = answer_to(row, ERROR_AT_CEILING_WIRE, &[], |url| async move {
        bounded_client().get(&url).await
    })
    .await;

    assert_eq!(observed.status, 500, "{row}: the send changed the status");
    assert_eq!(
        &*observed.body, ADMITTED,
        "{row}: the admitted error body must arrive whole"
    );
    assert!(
        is_status(&observed.converted, 500),
        "{row}: error_for_status returned {:?}",
        observed.converted
    );
}

/// An error status above the ceiling is the collection refusal, not a status
/// and not a partial response.
async fn final_error_status_above_the_ceiling_is_a_limit_failure() {
    let row = "error status above the ceiling";
    let result = bounded_result(row, ERROR_ABOVE_CEILING_WIRE).await;

    assert_client_limit(row, CEILING, result);
}

/// An error status whose body is cut short is the transport failure, with no
/// partial response.
async fn final_error_status_cut_short_is_a_transport_failure() {
    let row = "error status cut short";
    let result = bounded_result(row, ERROR_TRUNCATED_WIRE).await;

    match result {
        Err(RuntimeError::Client(ClientError::Transport)) => {}
        Err(other) => panic!("{row}: expected the transport failure, got {other:?}"),
        Ok(response) => panic!(
            "{row}: expected a failure, got {} bytes",
            response.body_bytes().len()
        ),
    }
}

/// A mock body that exactly fills [`CEILING`] and is not UTF-8.
const MOCK_AT_CEILING: &[u8] = b"\xffmock-binary-16!";

/// A mock answer within the ceiling converts whole: status, header, and bytes.
async fn mock_answer_within_the_ceiling_is_converted_whole() {
    let row = "mock at the ceiling";
    let url = "http://client-response.mock.test/at-ceiling";
    let canned = Response::bytes(503, MOCK_AT_CEILING)
        .expect("a valid mock status")
        .with_header("x-mock", "kept");
    let registered = mock::http(url).returns(canned);

    let fetched: Result<ClientResponse, RuntimeError> =
        within_bound(row, bounded_client().get(url)).await;
    let observed = answered(row, fetched.map(|response| observe(response, &["x-mock"])));

    registered.assert_called_once();
    assert_eq!(MOCK_AT_CEILING.len(), CEILING, "{row}: the fixture moved");
    assert_eq!(observed.status, 503, "{row}: the mock status changed");
    assert_eq!(
        &*observed.body, MOCK_AT_CEILING,
        "{row}: the mock body changed"
    );
    assert_eq!(
        *observed.first,
        [Some(Box::from(&b"kept"[..]))],
        "{row}: the mock header was lost"
    );
    assert!(
        is_decode(&observed.text, ClientDecode::Utf8),
        "{row}: text returned {:?} for a body that is not UTF-8",
        observed.text
    );
    assert!(
        is_status(&observed.converted, 503),
        "{row}: error_for_status returned {:?}",
        observed.converted
    );
}

/// A mock answer one byte above the ceiling is refused under the same typed
/// cause a real answer is.
async fn mock_answer_above_the_ceiling_is_refused() {
    let row = "mock above the ceiling";
    let url = "http://client-response.mock.test/above-ceiling";
    let canned = Response::bytes(200, vec![b'm'; CEILING + 1]).expect("a valid mock status");
    let registered = mock::http(url).returns(canned);

    let fetched: Result<ClientResponse, RuntimeError> =
        within_bound(row, bounded_client().get(url)).await;
    drop(registered);

    assert_client_limit(row, CEILING, fetched);
}

/// 2.T2
#[tokio::test]
async fn outbound_response_conversion_keeps_collection_and_mock_bounds() {
    final_error_status_at_the_ceiling_is_collected_whole().await;
    final_error_status_above_the_ceiling_is_a_limit_failure().await;
    final_error_status_cut_short_is_a_transport_failure().await;
    mock_answer_within_the_ceiling_is_converted_whole().await;
    mock_answer_above_the_ceiling_is_refused().await;
}
