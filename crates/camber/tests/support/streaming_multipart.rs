//! The wire bodies and reading handlers the streaming multipart route cases
//! share.
//!
//! Two roots drive `Router::multipart` — the routing component root and the
//! body/files component root — and both need the same three things: a body
//! framed the way a peer frames one, a handler that reads it to its end, and one
//! description of what came out. Stated once here, because a copy per root is a
//! copy that can come to describe a different body than the one it sent.

use super::http as wire;

use camber::RuntimeError;
use camber::http::mock::LifecycleController;
use camber::http::{MultipartField, MultipartStream, Request, Response};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The boundary every fixture body is framed with unless a row names its own.
pub const BOUNDARY: &str = "CamberBnd7";

/// The representation a fixture body framed with [`BOUNDARY`] declares itself
/// under, spelled once for every root that sends one.
///
/// [`content_type`] asserts the two agree, so a change to either side fails a
/// case instead of sending a declaration the body does not match.
pub const DECLARED: &str = "multipart/form-data; boundary=CamberBnd7";

/// What one field description reports where the part declared nothing.
pub const ABSENT: &str = "-";

/// One field a fixture body carries, exactly as the wire will spell it.
pub struct Field<'a> {
    pub name: &'a str,
    pub filename: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub data: &'a [u8],
}

impl<'a> Field<'a> {
    /// A form field that declares a name and nothing else.
    pub fn text(name: &'a str, data: &'a str) -> Self {
        Self {
            name,
            filename: None,
            content_type: None,
            data: data.as_bytes(),
        }
    }

    /// A form field that carries bytes under a name and nothing else.
    pub fn bytes(name: &'a str, data: &'a [u8]) -> Self {
        Self {
            name,
            filename: None,
            content_type: None,
            data,
        }
    }

    /// An uploaded file, with the filename and representation it declares.
    pub fn file(name: &'a str, filename: &'a str, content_type: &'a str, data: &'a [u8]) -> Self {
        Self {
            name,
            filename: Some(filename),
            content_type: Some(content_type),
            data,
        }
    }

    /// The header block this field puts on the wire.
    fn headers(&self) -> String {
        let mut block = format!("Content-Disposition: form-data; name=\"{}\"", self.name);
        if let Some(filename) = self.filename {
            block.push_str(&format!("; filename=\"{filename}\""));
        }
        if let Some(content_type) = self.content_type {
            block.push_str(&format!("\r\nContent-Type: {content_type}"));
        }
        block
    }
}

/// The opening one part puts on the wire before its data.
fn raw_opening(boundary: &str, headers: &str) -> String {
    format!("--{boundary}\r\n{headers}\r\n\r\n")
}

/// The opening one field puts on the wire before its data.
pub fn field_opening(boundary: &str, field: &Field<'_>) -> String {
    raw_opening(boundary, &field.headers())
}

/// Frame one multipart body from raw header blocks, ending with the epilogue
/// the row wants after the closing delimiter.
///
/// The framing loop lives here alone. A row about header spellings writes its
/// own block, and a row about what may follow the ending writes its own
/// epilogue, but neither restates how a part is delimited.
pub fn raw_multipart_body(boundary: &str, parts: &[(&str, &[u8])], epilogue: &[u8]) -> Vec<u8> {
    let mut wire = Vec::new();
    for (headers, data) in parts {
        wire.extend_from_slice(raw_opening(boundary, headers).as_bytes());
        wire.extend_from_slice(data);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"--");
    wire.extend_from_slice(boundary.as_bytes());
    wire.extend_from_slice(b"--");
    wire.extend_from_slice(epilogue);
    wire
}

/// Frame one multipart body exactly as a peer sends it.
pub fn multipart_body(boundary: &str, fields: &[Field<'_>]) -> Vec<u8> {
    let blocks: Vec<String> = fields.iter().map(Field::headers).collect();
    let parts: Vec<(&str, &[u8])> = blocks
        .iter()
        .zip(fields)
        .map(|(block, field)| (block.as_str(), field.data))
        .collect();
    raw_multipart_body(boundary, &parts, b"\r\n")
}

/// The representation one fixture body declares itself under.
pub fn content_type(boundary: &str) -> Box<str> {
    let declared = format!("multipart/form-data; boundary={boundary}").into_boxed_str();
    debug_assert!(
        boundary != BOUNDARY || &*declared == DECLARED,
        "DECLARED must spell what a body framed with BOUNDARY declares itself as"
    );
    declared
}

/// The handler future shape every fixture handler hands back.
///
/// Spelled out rather than inferred, so a fixture that stores several handlers
/// in one table has one type to name for all of them.
pub type HandlerFuture = Pin<Box<dyn Future<Output = Result<Response, RuntimeError>> + Send>>;

/// What one read field turned out to be, as one line of a description.
///
/// The chunk count is in it because a row's claim is about delivery as much as
/// content: a field delivered in one oversized chunk and the same field
/// delivered under its configured maximum describe differently here.
fn describe(name: &str, filename: &str, content_type: &str, bytes: usize, chunks: usize) -> String {
    format!("{name}/{filename}/{content_type}/{bytes}/{chunks}")
}

/// Send one multipart body to a served route and read the answer off the wire.
///
/// The declaration is [`BOUNDARY`]'s, which is the boundary [`multipart_body`]
/// frames with unless a caller names its own. A row sending a body framed under
/// a different delimiter is sending a body this declaration does not describe —
/// which is exactly what the structural-failure rows want.
pub fn upload(addr: SocketAddr, path: &str, body: &[u8]) -> wire::HttpResponse {
    let declared = content_type(BOUNDARY);
    wire::request_to_host_with_body(
        addr,
        "POST",
        path,
        "localhost",
        &[("Content-Type", declared.as_ref())],
        body,
    )
    .unwrap_or_else(|error| panic!("POST {path} did not complete: {error}"))
}

/// Read one field to its end and report how many bytes it carried.
pub async fn drain_field(field: &mut MultipartField<'_>) -> Result<usize, RuntimeError> {
    let mut bytes = 0;
    while let Some(chunk) = field.next_chunk().await? {
        bytes += chunk.len();
    }
    Ok(bytes)
}

/// Read every field still to come and report how many bytes they carried.
///
/// Reading to the end is what completes the framing: a handler that stopped at
/// the last field it wanted has left the closing delimiter unread, and the
/// session it hands back is incomplete rather than clean.
pub async fn drain_fields(stream: &mut MultipartStream) -> Result<usize, RuntimeError> {
    let mut bytes = 0;
    while let Some(mut field) = stream.next_field().await? {
        bytes += drain_field(&mut field).await?;
    }
    Ok(bytes)
}

/// A handler that reports its own failure without reading anything.
pub fn failing_handler(_request: &Request, _stream: MultipartStream) -> HandlerFuture {
    Box::pin(async move {
        Err(RuntimeError::BadRequest(
            "the handler declined this upload".into(),
        ))
    })
}

/// A handler that answers without reading its body at all.
pub fn silent_handler(_request: &Request, _stream: MultipartStream) -> HandlerFuture {
    Box::pin(async move { Response::text(200, "read nothing") })
}

/// A handler that reads its whole body and then reports its own failure.
///
/// The one precedence row where a handler error meets a clean terminal. Nothing
/// was left unread, so the answer carries the handler's own category and none of
/// the unread-body disposition every abandoned and failed row carries.
pub fn declining_reader(_request: &Request, stream: MultipartStream) -> HandlerFuture {
    Box::pin(async move {
        read_fields(stream).await?;
        Err(RuntimeError::BadRequest(
            "the handler declined what it had read".into(),
        ))
    })
}

/// Where one handler leaves its access handle before it returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Escape {
    /// Into a channel the case itself holds.
    Channel,
    /// Into a spawned task that waits for the case to release it.
    Task,
}

/// The handles one escaping handler hands out, and the gate its task waits on.
pub struct Escapes {
    handles: Mutex<Vec<MultipartStream>>,
    released: tokio::sync::Notify,
    reported: Mutex<Vec<Box<str>>>,
    reported_ready: tokio::sync::Notify,
}

impl Escapes {
    /// An escape holder with nothing handed over yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            handles: Mutex::new(Vec::new()),
            released: tokio::sync::Notify::new(),
            reported: Mutex::new(Vec::new()),
            reported_ready: tokio::sync::Notify::new(),
        })
    }

    /// Take the handle the last channel-escaping handler handed over.
    fn take_handle(&self) -> Option<MultipartStream> {
        self.handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
    }

    /// Keep the handle for the case to use once the handler has returned.
    fn keep_handle(&self, stream: MultipartStream) {
        self.handles
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(stream);
    }

    /// Record what one escaped task's handle answered.
    ///
    /// `notify_one` rather than `notify_waiters`: the waiter need not have
    /// reached its wait yet, and this wake is stored until it arrives.
    fn report(&self, reported: Box<str>) {
        self.reported
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(reported);
        self.reported_ready.notify_one();
    }

    /// Take one escaped task's report, if it has made one.
    fn taken_report(&self) -> Option<Box<str>> {
        self.reported
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pop()
    }
}

/// A handler that moves its access handle somewhere else and returns success.
pub fn escaping_handler(
    escape: Escape,
    escapes: &Arc<Escapes>,
) -> impl Fn(&Request, MultipartStream) -> HandlerFuture + Send + Sync + 'static {
    let escapes = Arc::clone(escapes);
    move |_request: &Request, stream: MultipartStream| {
        let escapes = Arc::clone(&escapes);
        Box::pin(async move {
            match escape {
                Escape::Channel => escapes.keep_handle(stream),
                Escape::Task => {
                    tokio::spawn(escaped_read(stream, Arc::clone(&escapes)));
                }
            }
            Response::text(200, "handed off")
        })
    }
}

/// Use one escaped handle once the case has released it, and report what it
/// answered.
async fn escaped_read(mut stream: MultipartStream, escapes: Arc<Escapes>) {
    escapes.released.notified().await;
    let reported: Box<str> = match stream.next_field().await {
        Ok(_) => "the escaped handle still read a field".into(),
        Err(error) => format!("{error}").into(),
    };
    escapes.report(reported);
}

/// Assert one escaped handle answers immediately and reads nothing more.
///
/// `polled` is the payload-frame count taken when the handler completed: an
/// escaped handle that reached the transport again would raise it.
pub async fn assert_escaped_inert(
    escapes: &Arc<Escapes>,
    escape: Escape,
    polled: usize,
    controller: &LifecycleController,
    path: &str,
    bound: Duration,
) {
    match escape {
        Escape::Channel => assert_channel_handle_inert(escapes, path, bound).await,
        Escape::Task => assert_task_handle_inert(escapes, path, bound).await,
    }
    assert_eq!(
        controller.multipart_observed().body_frames_polled(),
        polled,
        "{path}: an escaped handle polls no further payload"
    );
}

/// Assert the handle a handler left in the case's own channel is inert.
async fn assert_channel_handle_inert(escapes: &Arc<Escapes>, path: &str, bound: Duration) {
    let mut handle = escapes
        .take_handle()
        .expect("the handler handed its access handle to the case");
    // Bounded: a handle whose revocation regressed is awaiting a driver that no
    // longer exists, and that must fail the case rather than park it.
    let refused = tokio::time::timeout(bound, handle.next_field())
        .await
        .unwrap_or_else(|_| panic!("{path}: an escaped handle answers within its bound"));
    assert!(
        refused.is_err(),
        "{path}: an escaped handle is inert once admission is revoked"
    );
}

/// Assert the handle a handler left in a spawned task is inert.
async fn assert_task_handle_inert(escapes: &Arc<Escapes>, path: &str, bound: Duration) {
    // `notify_one` rather than `notify_waiters`: the handler has returned, but
    // the task it spawned need not have reached its wait yet, and a wake sent
    // to no one is a wake lost. This one is stored until the task arrives.
    escapes.released.notify_one();
    let reported = tokio::time::timeout(bound, escaped_report(escapes))
        .await
        .expect("the escaped task reported");
    assert!(
        reported.contains("multipart"),
        "{path}: the escaped task was refused as a multipart failure: {reported}"
    );
}

/// Wait for the escaped task's own report.
///
/// Parked on the report's own signal rather than spun on: a case that is going
/// to fail waits out its bound asleep instead of holding a core.
async fn escaped_report(escapes: &Arc<Escapes>) -> Box<str> {
    loop {
        if let Some(reported) = escapes.taken_report() {
            return reported;
        }
        escapes.reported_ready.notified().await;
    }
}

/// Read every field to its end and describe them in wire order.
pub async fn read_fields(mut stream: MultipartStream) -> Result<String, RuntimeError> {
    let mut described: Vec<String> = Vec::new();
    while let Some(mut field) = stream.next_field().await? {
        let name = field.name().to_owned();
        let filename = field.filename().unwrap_or(ABSENT).to_owned();
        let representation = field.content_type().unwrap_or(ABSENT).to_owned();
        let mut bytes = 0;
        let mut chunks = 0;
        while let Some(chunk) = field.next_chunk().await? {
            assert!(!chunk.is_empty(), "every delivered chunk carries bytes");
            bytes += chunk.len();
            chunks += 1;
        }
        described.push(describe(&name, &filename, &representation, bytes, chunks));
    }
    Ok(described.join(","))
}

/// A handler that reads its whole body and reports what it saw.
///
/// The tag and the matched route parameter travel into the answer so a case can
/// tell which registration answered without a second observation of its own.
pub fn reading_handler(
    tag: &'static str,
    param: &'static str,
    handled: &Arc<AtomicUsize>,
) -> impl Fn(&Request, MultipartStream) -> HandlerFuture + Send + Sync + 'static {
    let handled = Arc::clone(handled);
    move |request: &Request, stream: MultipartStream| {
        let handled = Arc::clone(&handled);
        let matched: Box<str> = request.param(param).unwrap_or(ABSENT).into();
        Box::pin(async move {
            handled.fetch_add(1, Ordering::SeqCst);
            let described = read_fields(stream).await?;
            Response::text(200, &format!("{tag}|{matched}|{described}"))
        })
    }
}
