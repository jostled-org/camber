//! Streaming 5.T6: the documented streaming request compiles and runs as
//! written.
//!
//! The row is the shape the reference page shows: describe a request, send
//! it through the streaming terminal, convert its status, read its headers,
//! hand each chunk to a sink the application owns, and close. It names only
//! `camber` types. An application that had to name a transport type to write
//! this would not compile here.
//!
//! The answer comes from the HTTP mock, through the real body owner. A mock
//! resolves nothing and dials nothing, so this is a contract of the public
//! shape. It is not proof of framing, confinement, or runtime ownership.
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | The published signatures hold | `assert_published_signatures` | alter a published signature |
//! | Response, body, and their futures are `Send` | `assert_send`, `require_send`, the spawned fetch | hold a non-`Send` value across an await |
//! | Only `camber` names are needed | this module's imports | expose a backend type requirement |
//! | Headers are raw bytes | `fetch_into` | return header values as text |
//! | Debug shows no header value | `fetch_into` | derive `Debug` |
//! | Every chunk reaches the sink once, in order | `streamed_object_reaches_the_sink` | drop or repeat a chunk |
//! | An error status ends the fetch before the sink | `error_status_writes_nothing` | make conversion a no-op |
//! | A full sink ends the fetch with the application's error | `full_sink_ends_the_fetch` | hand the sink a chunk out of order, or hide the sink's error |
//!
//! A mock has no body-owner observer, so no row here reads the release of a
//! dropped body. `AfterHead::UnreadDrop` in
//! `component_http_routing/client_streaming.rs` proves that.

use crate::probes::{assert_send, require_send};

use camber::RuntimeError;
use camber::http;
use camber::http::Bytes;
use camber::http::Client;
use camber::http::ClientBody;
use camber::http::ClientError;
use camber::http::ClientStreamResponse;
use camber::http::Response;
use camber::http::mock;
use std::time::{Duration, Instant};

/// The request maximum of the example.
const OBJECT_LIMIT: usize = 64 * 1024;

/// How long the example gives one fetch.
const FETCH_BUDGET: Duration = Duration::from_secs(30);

/// The object the mock serves, its media type, and one more header.
const OBJECT: &[u8] = b"streamed-object-payload";
const OBJECT_TYPE: &str = "application/octet-stream";
const OBJECT_TAG: (&str, &str) = ("etag", "tag-sentinel-71c0");

/// Why one fetch ended without the whole object in its sink.
#[derive(Debug)]
enum FetchError {
    /// The client refused the request or failed the response.
    Client(RuntimeError),
    /// The sink has no room for the next chunk.
    SinkFull,
}

impl From<RuntimeError> for FetchError {
    fn from(error: RuntimeError) -> Self {
        Self::Client(error)
    }
}

/// The application's sink: a finite buffer that counts what it was given.
///
/// It stands for an object writer. Camber does not buffer for it and does not
/// own it.
struct CountingSink {
    capacity: usize,
    written: Vec<u8>,
    chunks: usize,
}

impl CountingSink {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            written: Vec::new(),
            chunks: 0,
        }
    }

    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), FetchError> {
        if self.written.len() + chunk.len() > self.capacity {
            return Err(FetchError::SinkFull);
        }
        self.written.extend_from_slice(&chunk);
        self.chunks += 1;
        Ok(())
    }
}

/// What one fetch read from the head before it read the body.
#[derive(Debug, Eq, PartialEq)]
struct Fetched {
    status: u16,
    media_type: Option<Box<[u8]>>,
    tags: Box<[Box<[u8]>]>,
}

/// The documented shape: request → headers → next → application sink → close.
async fn fetch_into(
    client: &Client,
    source_url: &str,
    deadline: Instant,
    sink: &mut CountingSink,
) -> Result<Fetched, FetchError> {
    let sending = client
        .request("GET", source_url)?
        .response_limit(OBJECT_LIMIT)?
        .deadline(deadline)
        .send_streaming();
    let response: ClientStreamResponse = require_send(sending).await?.error_for_status()?;
    assert_send::<ClientStreamResponse>();

    let rendering = format!("{response:?}");
    assert!(
        !rendering.contains(OBJECT_TAG.1) && !rendering.contains(OBJECT_TYPE),
        "Debug shows a header value: {rendering}"
    );
    let fetched = Fetched {
        status: response.status(),
        media_type: response.header("content-type").map(Box::from),
        tags: response
            .headers()
            .filter(|(name, _): &(&str, &[u8])| name.eq_ignore_ascii_case(OBJECT_TAG.0))
            .map(|(_, value): (&str, &[u8])| Box::from(value))
            .collect(),
    };

    let mut body: ClientBody = response.into_body();
    assert_send::<ClientBody>();
    while let Some(chunk) = require_send(body.next()).await? {
        sink.write_chunk(chunk).await?;
    }
    require_send(body.close()).await?;
    Ok(fetched)
}

/// Pin the published signatures, so a changed one fails here by name.
fn assert_published_signatures() {
    let _: fn(&ClientStreamResponse) -> u16 = ClientStreamResponse::status;
    let _: for<'a> fn(&'a ClientStreamResponse, &str) -> Option<&'a [u8]> =
        ClientStreamResponse::header;
    let _: fn(ClientStreamResponse) -> Result<ClientStreamResponse, RuntimeError> =
        ClientStreamResponse::error_for_status;
    let _: fn(ClientStreamResponse) -> ClientBody = ClientStreamResponse::into_body;
}

/// Run one fetch as a task of its own, and return its result and its sink.
///
/// The task owns the client and the sink. Spawning it is the `Send` proof of
/// the whole shape, and joining it is the row's cleanup.
async fn fetch(url: &'static str, capacity: usize) -> (Result<Fetched, FetchError>, CountingSink) {
    let task = tokio::spawn(async move {
        let client: Client = http::client();
        let mut sink = CountingSink::with_capacity(capacity);
        let deadline = Instant::now() + FETCH_BUDGET;
        let result = fetch_into(&client, url, deadline, &mut sink).await;
        (result, sink)
    });
    task.await
        .unwrap_or_else(|joined| panic!("the fetch task did not complete: {joined}"))
}

/// A mock answer with this status that serves the object and its headers.
fn object_answer(status: u16) -> Response {
    Response::bytes(status, OBJECT)
        .unwrap_or_else(|error| panic!("the mock answer was refused: {error:?}"))
        .with_content_type(OBJECT_TYPE)
        .with_header(OBJECT_TAG.0, OBJECT_TAG.1)
}

/// The whole object reaches the sink once and in order, behind a head that
/// was read first.
async fn streamed_object_reaches_the_sink() {
    const URL: &str = "http://objects.streaming.test/whole";
    let registered = mock::http(URL).returns(object_answer(200));

    let (result, sink) = fetch(URL, OBJECT_LIMIT).await;
    registered.assert_called_once();
    drop(registered);

    let fetched = result.unwrap_or_else(|error| panic!("the fetch failed: {error:?}"));
    assert_eq!(
        fetched,
        Fetched {
            status: 200,
            media_type: Some(OBJECT_TYPE.as_bytes().into()),
            tags: Box::new([OBJECT_TAG.1.as_bytes().into()]),
        },
        "the head the fetch read"
    );
    assert_eq!(sink.written, OBJECT, "the bytes the sink was given");
}

/// An error status ends the fetch at the conversion. The sink is given
/// nothing.
async fn error_status_writes_nothing() {
    const URL: &str = "http://objects.streaming.test/missing";
    let registered = mock::http(URL).returns(object_answer(404));

    let (result, sink) = fetch(URL, OBJECT_LIMIT).await;
    drop(registered);

    assert!(
        matches!(
            result,
            Err(FetchError::Client(RuntimeError::Client(
                ClientError::Status(404)
            )))
        ),
        "the fetch of an error status returned {result:?}"
    );
    assert_eq!(
        sink.chunks, 0,
        "the sink was given a chunk of an error body"
    );
}

/// A sink with no room ends the fetch with the application's own error. The
/// body is dropped unread, which needs no close.
async fn full_sink_ends_the_fetch() {
    const URL: &str = "http://objects.streaming.test/too-big-for-the-sink";
    let registered = mock::http(URL).returns(object_answer(200));

    let (result, sink) = fetch(URL, OBJECT.len() - 1).await;
    registered.assert_called_once();
    drop(registered);

    assert!(
        matches!(result, Err(FetchError::SinkFull)),
        "the fetch into a full sink returned {result:?}"
    );
    assert!(
        OBJECT.starts_with(&sink.written),
        "the sink was given bytes that are not the object's prefix: {:?}",
        sink.written
    );
}

/// 5.T6
#[tokio::test]
async fn outbound_streaming_example_compiles_and_runs() {
    assert_published_signatures();
    streamed_object_reaches_the_sink().await;
    error_status_writes_nothing().await;
    full_sink_ends_the_fetch().await;
}
