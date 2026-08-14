//! What a rejection mapper was handed, kept so a table row can assert on it.
//!
//! Several suites drive refusals through a live router and read back the
//! category, default status, safe message, and established context their
//! producer fixed. The observation shape is stated once here: a copy per suite
//! is a copy that can drift from the contract it exists to prove.

use camber::RuntimeError;
#[cfg(feature = "ws")]
use camber::http::WsConn;
use camber::http::{
    NegotiatedResponseMetadata, Rejection, RejectionContext, RejectionKind, RejectionProtocol,
    Request, Response,
};
use serde::Deserialize;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::http::HttpResponse;

/// The exact body every redacted internal failure answers with.
pub const REDACTED_BODY: &str = "internal server error";

/// The body a refusal that is about service state answers with.
pub const UNAVAILABLE_BODY: &str = "service unavailable";

/// The fixed sentence every rejection event is recorded under.
///
/// The `message=` prefix is part of it: the capture writes fields as
/// `name=value`, so a needle without it would match the word wherever it
/// appeared, including inside a cause. Two roots spelled this out, and a copy
/// that drifted would go on finding nothing while reporting that production
/// recorded nothing.
pub const REJECTION_MESSAGE: &str = "message=request rejected";

/// The fixed sentence every completion event is recorded under.
pub const COMPLETION_MESSAGE: &str = "message=request completed";

/// The status the classification tables collapse every category onto.
///
/// Every category answers the wire with this one number, so what a row reads
/// back from the journal is the producer's own classification rather than
/// something it could have re-derived from the response. Three roots declared
/// it, and three numbers that drifted apart would be three tables proving
/// different things under one name.
pub const COLLAPSED_STATUS: u16 = 418;

/// A JSON body that is not the shape any validated route here parses.
pub const MALFORMED_JSON: &str = "{\"id\":";

/// A multipart body that never frames a part.
///
/// Beside [`MALFORMED_JSON`] for the reason [`Ticket`] is: a body is only
/// malformed with respect to a parser, and the two roots that declared this
/// could come to disagree about which parser it is malformed for.
pub const MALFORMED_MULTIPART: &str = "--not-the-boundary\r\nnothing here\r\n";

/// The representation a multipart body declares itself under.
///
/// The boundary is the caller's, because the boundary is what a root varies;
/// the parameter name, the separator, and the media type are not. Two roots
/// wrote this one `format!` out, and a copy that spelled the parameter
/// differently would send a representation the parser answers about rather than
/// the body.
///
/// Sealed: every caller sends the value as it stands.
pub fn multipart_content_type(boundary: &str) -> Box<str> {
    format!("multipart/form-data; boundary={boundary}").into_boxed_str()
}

/// The client-safe message an application refusal declares for itself.
///
/// Two roots declared this same sentence, and a row asserts it on the wire as
/// well as in the journal: a copy that drifted would have the two halves of one
/// claim proving different messages.
pub const DECLARED_MESSAGE: &str = "field id is required";

/// The WebSocket version a rejection policy offers on a refusal it did not shape.
///
/// Not `13`, which is the version the framework requires: the whole point of the
/// value is that enforcement has something to correct. A copy that drifted to
/// `13` would leave every correction row passing without correcting anything.
pub const MAPPER_VERSION: &str = "7";

/// The shape those routes do parse.
///
/// The deserialize target the malformed body fails against. Stated beside it,
/// because the two are one fixture: a body is only malformed with respect to
/// something, and a root that kept its own target could come to disagree about
/// what [`MALFORMED_JSON`] is malformed for.
#[derive(Debug, Deserialize)]
pub struct Ticket {
    pub id: Box<str>,
}

/// A header name `with_header` accepts and Hyper cannot represent.
///
/// This spelling is the production contract: it is what provokes the refusal
/// every suite names it for, so a copy that drifted would go on being sent and
/// quietly stop provoking anything.
pub const UNREPRESENTABLE_HEADER: &str = "X-Broken\nName";

/// The header a rejection response names its request under.
pub const REQUEST_ID_HEADER: &str = "X-Request-Id";

/// One mapper invocation, kept as the values that mapper was handed.
///
/// Recorded rather than asserted inside the closure so a row's failure names
/// the whole observation instead of the first field that differed. `Debug` is
/// the only capability any reader wants: every caller borrows one of these and
/// reads its fields, and a failure that has to name the whole observation is
/// what `Debug` is here for.
#[derive(Debug)]
pub struct Observed {
    pub origin: &'static str,
    pub kind: RejectionKind,
    pub status: u16,
    pub message: Box<str>,
    pub method: Box<str>,
    pub raw_path: Box<str>,
    pub route: Option<Box<str>>,
    pub protocol: Option<RejectionProtocol>,
    pub content_type: Option<Box<str>>,
    pub subprotocol: Option<Box<str>>,
    pub allow: Option<Box<str>>,
    pub remote: Option<IpAddr>,
    pub request_id: Box<str>,
}

/// Every mapper invocation one fixture saw, in order.
pub type Journal = Arc<Mutex<Vec<Observed>>>;

/// An empty journal, for a mapper that has recorded nothing yet.
pub fn journal() -> Journal {
    Arc::new(Mutex::new(Vec::new()))
}

/// The `Allow` value a rejection offers as a safe default, if it offers one.
fn default_allow(rejection: &Rejection) -> Option<Box<str>> {
    rejection
        .headers()
        .find(|(name, _)| name.eq_ignore_ascii_case("Allow"))
        .map(|(_, value)| Box::from(value))
}

/// Record everything one mapper invocation was handed.
fn observe(
    journal: &Journal,
    origin: &'static str,
    rejection: &Rejection,
    context: &RejectionContext,
) {
    journal
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(Observed {
            origin,
            kind: rejection.kind(),
            status: rejection.status(),
            message: rejection.message().into(),
            method: context.method().into(),
            raw_path: context.raw_path().into(),
            route: context.route().map(Box::from),
            protocol: context
                .negotiated()
                .map(NegotiatedResponseMetadata::protocol),
            content_type: context
                .negotiated()
                .and_then(NegotiatedResponseMetadata::content_type)
                .map(Box::from),
            subprotocol: context
                .negotiated()
                .and_then(NegotiatedResponseMetadata::subprotocol)
                .map(Box::from),
            allow: default_allow(rejection),
            remote: context.remote_addr(),
            request_id: context.request_id().as_str().into(),
        });
}

/// A mapper that records what it was given and answers under `status`.
///
/// `None` keeps the producer's own default status. Both public forms delegate
/// here, because they differ in that one expression and nothing else: a field
/// added to the recorded observation must not be able to reach one of them and
/// not the other.
fn mapper(
    journal: &Journal,
    origin: &'static str,
    status: Option<u16>,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    let journal = Arc::clone(journal);
    move |rejection: &Rejection, context: &RejectionContext| {
        observe(&journal, origin, rejection, context);
        Response::text(
            status.unwrap_or_else(|| rejection.status()),
            rejection.message(),
        )
    }
}

/// A mapper that records what it was given and answers with the safe defaults.
pub fn recording_mapper(
    journal: &Journal,
    origin: &'static str,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    mapper(journal, origin, None)
}

/// A mapper that records what it was given and answers with one fixed status.
///
/// Every category collapses onto the same wire status, so what reaches the
/// journal is the producer's own classification rather than something a reader
/// could have re-derived from the response.
pub fn collapsing_mapper(
    journal: &Journal,
    origin: &'static str,
    status: u16,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    mapper(journal, origin, Some(status))
}

/// Take everything the mappers have recorded so far.
///
/// Sealed: a caller counts what it took or reads one entry out of it, and
/// nothing appends to the record of what already happened.
pub fn drain(journal: &Journal) -> Box<[Observed]> {
    std::mem::take(&mut *journal.lock().unwrap_or_else(|error| error.into_inner()))
        .into_boxed_slice()
}

/// The one observation a row expects, or a failure naming what it got instead.
///
/// A second invocation is the failure this whole boundary is guarded against,
/// so the failure carries both observations rather than only their count: two
/// refusals from one request are told apart by what each was handed.
pub fn only(journal: &Journal, label: &str) -> Observed {
    let seen = drain(journal);
    assert_eq!(
        seen.len(),
        1,
        "{label}: one refusal invokes one mapper once, not {seen:?}"
    );
    seen.into_vec().remove(0)
}

/// A handler that reports it ran and answers with the same body either way.
///
/// The counter is what makes a refusal's claim about non-entry load-bearing:
/// every row that says the refusal preceded the terminal is only saying so
/// because this stayed at zero. Two roots wrote it, differing in the body they
/// answered with and in nothing else.
pub fn counting_handler(
    handled: &Arc<AtomicUsize>,
    body: &'static str,
) -> impl Fn(&Request) -> std::future::Ready<Result<Response, RuntimeError>> + Send + Sync + 'static
{
    let handled = Arc::clone(handled);
    move |_request: &Request| {
        handled.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Response::text(200, body))
    }
}

/// The header an echoing handler names its request under.
///
/// A policy's record of which request it was asked about is only meaningful
/// against the identity the handler went on to report, so the answer carries it.
pub const HANDLED_REQUEST_ID_HEADER: &str = "X-Handled-Request-Id";

/// A handler that answers with exactly the body it was given, and names the
/// identity it ran under.
///
/// The terminal every admitted body is driven into: a `200` carrying the same
/// bytes back is what says the whole body reached the handler. Three roots wrote
/// it out byte for byte.
///
/// The body is taken before the future because the request is borrowed: a
/// handler that carried the borrow into an `async move` would hold it across the
/// await point the router owns.
pub fn echo_handler(request: &Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let body: Box<str> = request.body().into();
    let request_id: Box<str> = request.request_id().as_str().into();
    Box::pin(async move {
        Response::text(200, &body)
            .expect("valid echo status")
            .with_header(HANDLED_REQUEST_ID_HEADER, &request_id)
    })
}

/// [`echo_handler`], reporting every entry.
///
/// The counter carries [`counting_handler`]'s claim onto the echoing terminal: a
/// row saying a refusal preceded the handler is only saying so because this
/// stayed where it was.
pub fn counting_echo(
    handled: &Arc<AtomicUsize>,
) -> impl Fn(&Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static {
    let handled = Arc::clone(handled);
    move |request: &Request| {
        handled.fetch_add(1, Ordering::SeqCst);
        echo_handler(request)
    }
}

/// The body a handler answers with when the answer itself cannot be sent.
const UNSENDABLE_BODY: &str = "unsendable";

/// A handler that reports it ran and answers with a head Hyper cannot carry.
///
/// [`counting_handler`] with the one header `with_header` accepts and the wire
/// cannot represent. The counter is the same claim it makes there: a row saying
/// the refusal came after the handler is only saying so because this moved.
/// Two roots wrote it out byte for byte.
pub fn unrepresentable_handler(
    handled: &Arc<AtomicUsize>,
) -> impl Fn(&Request) -> std::future::Ready<Result<Response, RuntimeError>> + Send + Sync + 'static
{
    let counted = counting_handler(handled, UNSENDABLE_BODY);
    move |request: &Request| {
        let answered = counted(request).into_inner();
        std::future::ready(
            answered.map(|response| response.with_header(UNREPRESENTABLE_HEADER, "present")),
        )
    }
}

/// A WebSocket handler that reports whether it was ever entered.
///
/// [`counting_handler`]'s counterpart for the handshake rows. It answers
/// nothing: what every row that names it asserts is that the count stayed at
/// zero, so a refusal decided before any `101` was committed. Two roots wrote
/// it.
#[cfg(feature = "ws")]
pub fn counting_ws_handler(
    entries: &Arc<AtomicUsize>,
) -> impl Fn(&Request, WsConn) -> Result<(), RuntimeError> + Send + Sync + 'static {
    let entries = Arc::clone(entries);
    move |_request: &Request, _socket: WsConn| {
        entries.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A handler that parses [`Ticket`] out of the body and answers with its field.
///
/// The terminal every JSON row is driven at. It answers `200` only when the
/// parse succeeded, so a malformed body's refusal is the parser's own and not
/// something this handler decided. Two roots wrote it byte for byte.
///
/// The parse runs before the future because the request is borrowed: a handler
/// that carried the borrow into an `async move` would hold it across the await
/// point the router owns.
pub fn ticket_handler()
-> impl Fn(&Request) -> std::future::Ready<Result<Response, RuntimeError>> + Send + Sync + 'static {
    |request: &Request| {
        let parsed: Result<Ticket, RuntimeError> = request.json();
        std::future::ready(parsed.and_then(|ticket| Response::text(200, &ticket.id)))
    }
}

/// A handler that answers with how many parts a multipart body framed.
///
/// [`ticket_handler`]'s multipart sibling, and the same claim: a `200` means the
/// parser accepted the body, so a refusal below it is the parser's own. Two
/// roots wrote it byte for byte.
pub fn multipart_count_handler()
-> impl Fn(&Request) -> std::future::Ready<Result<Response, RuntimeError>> + Send + Sync + 'static {
    |request: &Request| {
        let parsed = request.multipart().map(|reader| reader.parts().len());
        std::future::ready(parsed.and_then(|count| Response::text(200, &count.to_string())))
    }
}

/// Name the request on a response a configured policy built.
///
/// A configured policy owns every header of what it returns, so nothing adds the
/// identifier for it. Three policies wrote this tail, each under the same three
/// lines of reasoning; a fourth that forgot it would answer a refusal the
/// operator cannot correlate against any record.
pub fn naming(
    answered: Result<Response, RuntimeError>,
    context: &RejectionContext,
) -> Result<Response, RuntimeError> {
    answered.map(|response| response.with_header(REQUEST_ID_HEADER, context.request_id().as_str()))
}

/// Assert nothing a refusal kept private reached the peer.
///
/// The whole raw response is searched, head and body alike: a cause that leaked
/// into a header is as much a leak as one that reached the body. The lossy read
/// is borrowed rather than owned — these responses are valid UTF-8, so a copy of
/// them would buy nothing.
///
/// An empty list is refused first, for the reason
/// [`super::rejection_metrics::assert_bounded_rejection_labels`] refuses an
/// empty scrape: this is a leak assertion, and one that was handed nothing to
/// look for passes over every response there is. A vacuous pass here is a leak
/// nobody looked for.
pub fn assert_no_private_text(response: &HttpResponse, private: &[&str], label: &str) {
    assert!(
        !private.is_empty(),
        "{label}: no private text was declared, so no leak can be found"
    );
    let raw = String::from_utf8_lossy(response.raw());
    for text in private {
        assert!(
            !raw.contains(text),
            "{label}: the private text {text:?} reached the peer: {raw}"
        );
    }
}

/// What one request's participants recorded, in the order they ran.
///
/// Stated here beside the journal because several suites prove stage order the
/// same way, and a copy per suite is a copy that can come to disagree about the
/// order it exists to state.
pub type Trail = Arc<Mutex<Vec<&'static str>>>;

/// Record one participant reaching its stage.
pub fn mark(trail: &Trail, marker: &'static str) {
    trail
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(marker);
}

/// Take everything recorded so far, leaving the trail empty for the next row.
///
/// Sealed: a row compares what it took against a declared order and never
/// appends to it.
pub fn take(trail: &Trail) -> Box<[&'static str]> {
    std::mem::take(&mut *trail.lock().unwrap_or_else(|error| error.into_inner())).into_boxed_slice()
}

/// Record one participant reaching its stage, then answer through `inner`.
///
/// Five roots wrote this combinator, each wrapping a mapper of its own: one that
/// only marks, one that marks and records, one that marks and collapses. They
/// differ in the mapper they delegate to and in nothing else, so the delegation
/// is stated once and the mapper stays the caller's.
///
/// The mark is taken before `inner` runs, because the trail's claim is order: a
/// mark written afterwards would place policy after whatever the mapper itself
/// recorded.
pub fn marking<M>(
    trail: &Trail,
    marker: &'static str,
    inner: M,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
where
    M: Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static,
{
    let trail = Arc::clone(trail);
    move |rejection: &Rejection, context: &RejectionContext| {
        mark(&trail, marker);
        inner(rejection, context)
    }
}

/// The identifier a rejection response carries, checked for shape.
pub fn request_id_of(response: &HttpResponse, label: &str) -> Box<str> {
    let header = response.header(REQUEST_ID_HEADER);
    assert_request_id_shape(header, label).into()
}

/// The shape one minted identifier has, whatever carried it.
///
/// Both halves are the claim: 32 digits and lowercase hexadecimal. A caller that
/// checked only the length would accept an inbound header a peer chose, which is
/// application data rather than Camber authority — and that is exactly what
/// these assertions exist to tell apart.
///
/// Reached through [`request_id_of`] or [`assert_established`] whenever the
/// identifier is still inside a response or a record, because pulling it out is
/// the step those two own. A root calls this directly only when a participant
/// handed it the identifier already — a mapper that recorded what it saw.
pub fn assert_request_id_shape<'a>(id: Option<&'a str>, label: &str) -> &'a str {
    let id = id.unwrap_or_else(|| panic!("{label}: nothing carried a request identifier"));
    assert_eq!(id.len(), 32, "{label}: identifier length");
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label}: identifier is lowercase hexadecimal: {id:?}"
    );
    id
}

/// Assert one refusal reached the one fixed redacted fallback.
///
/// Four claims, and all four are about the same answer: the status, the
/// representation, an identifier the peer can quote back, and the exact body.
/// Two roots wrote them out, and a copy that dropped one would report the
/// fallback as reached by a response that was only half of it.
pub fn assert_fixed_fallback(response: &HttpResponse, label: &str) {
    assert_eq!(response.status, 500, "{label}: status");
    assert_eq!(
        response.header("content-type"),
        Some("text/plain"),
        "{label}: content type"
    );
    request_id_of(response, label);
    assert_eq!(response.text().as_ref(), REDACTED_BODY, "{label}: body");
}

/// The established context one row expects its refusal to have been mapped with.
///
/// Named fields rather than positional arguments: five of the six are `Option`s
/// or strings, and a row that transposed two of them would go on asserting
/// something.
pub struct Established<'a> {
    pub method: &'a str,
    pub raw_path: &'a str,
    pub route: Option<&'a str>,
    pub protocol: Option<RejectionProtocol>,
    pub content_type: Option<&'a str>,
}

/// Assert what one refusal's producer had established when it was mapped.
///
/// The declared five, plus two that hold of every refusal a peer reached
/// Camber over: the transport named that peer, and the request carries a
/// well-shaped identity. Those two are not parameters because no row may opt out
/// of them — a refusal with no peer and no identity is one nothing can be
/// correlated against, whatever else it established. Three tables across two
/// roots wrote this block out, and each copy could drop a line the others kept.
pub fn assert_established(seen: &Observed, expected: &Established<'_>, label: &str) {
    assert_eq!(seen.method.as_ref(), expected.method, "{label}: method");
    assert_eq!(
        seen.raw_path.as_ref(),
        expected.raw_path,
        "{label}: raw path"
    );
    assert_eq!(seen.route.as_deref(), expected.route, "{label}: route");
    assert_eq!(seen.protocol, expected.protocol, "{label}: dispatch class");
    assert_eq!(
        seen.content_type.as_deref(),
        expected.content_type,
        "{label}: negotiated representation"
    );
    assert!(seen.remote.is_some(), "{label}: the transport named a peer");
    assert_request_id_shape(Some(seen.request_id.as_ref()), label);
}

/// What one collapsed-classification row expects its producer to have kept.
pub struct Collapsed<'a> {
    pub kind: RejectionKind,
    pub status: u16,
    pub message: &'a str,
}

/// Assert what one refusal's producer classified it as.
///
/// Three claims and one subject: the category, the status the producer would
/// have answered with, and the client-safe message it fixed. Thirteen rows
/// across the roots wrote the triple out by hand, because it used to be
/// reachable only through [`assert_collapsed`] — which also asserts the wire
/// status, and a row whose response is not collapsed cannot ask for that.
///
/// Split rather than parameterised: the wire claim and the classification claim
/// are about different things, and a flag deciding whether one of them ran would
/// let a row opt out of it without saying so.
pub fn assert_classification(seen: &Observed, expected: &Collapsed<'_>, label: &str) {
    assert_eq!(seen.kind, expected.kind, "{label}: category");
    assert_eq!(seen.status, expected.status, "{label}: default status");
    assert_eq!(
        seen.message.as_ref(),
        expected.message,
        "{label}: safe message"
    );
}

/// Assert one row's refusal collapsed onto the wire status and kept its own
/// classification.
///
/// The wire status is the same number for every row, so what the journal
/// carries is the producer's classification rather than something a reader
/// could have re-derived from the response. Two roots wrote this block out
/// under the same four message strings.
///
/// The observation is handed back rather than dropped, because each table makes
/// further claims of its own about the row it just read.
pub fn assert_collapsed(
    journal: &Journal,
    response: &HttpResponse,
    label: &str,
    expected: &Collapsed<'_>,
) -> Observed {
    assert_eq!(
        response.status, COLLAPSED_STATUS,
        "{label}: every category collapses onto one wire status"
    );
    let seen = only(journal, label);
    assert_classification(&seen, expected, label);
    seen
}

/// The innermost private cause [`two_level_failure`] carries.
pub const ROOT_CAUSE: &str = "signing key unreadable at /var/lib/camber/keys";

/// The cause wrapped around it.
pub const MIDDLE_CAUSE: &str = "credential refresh failed";

/// The innermost cause, as an error a chain can be built over.
#[derive(Debug)]
struct RootCause;

impl std::fmt::Display for RootCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(ROOT_CAUSE)
    }
}

impl std::error::Error for RootCause {}

/// The cause wrapped around [`RootCause`], reporting it as its source.
#[derive(Debug)]
struct MiddleCause(RootCause);

impl std::fmt::Display for MiddleCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MIDDLE_CAUSE)
    }
}

impl std::error::Error for MiddleCause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// A handler failure with a known two-level source chain beneath it.
///
/// The subject of every claim about what an operator is shown and a peer is
/// not: both causes must reach the record, in order, through the `cause` field
/// and nowhere else. Two roots built the same two-level chain out of their own
/// error types, so the depth this exercises could drift between them while both
/// went on calling it a chain.
pub fn two_level_failure() -> RuntimeError {
    RuntimeError::Io(std::io::Error::other(MiddleCause(RootCause)))
}

/// A handler failure with a single known cause beneath it.
///
/// The shallow half of the same claim: a record that showed one cause for both
/// depths would be showing the chain's head rather than the chain.
pub fn one_level_failure() -> RuntimeError {
    RuntimeError::Io(std::io::Error::other(RootCause))
}
