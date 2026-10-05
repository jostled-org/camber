//! Two tonic services answering every native RPC form, and the wire helpers
//! the rows that drive them share.
//!
//! The four methods of `greeter.NativeForms` share one behaviour in
//! [`FormsService`], so a row that compares forms compares transports and
//! nothing else. Each method counts its own entry, echoes the request's form
//! metadata onto its response head, and answers a request named [`FAIL_NAME`]
//! with a typed status whose metadata names the form. A streaming response
//! fails only after it has produced a message, so its status can only travel in
//! trailers.
//!
//! [`ScriptedForms`] answers the same four methods under a [`FormScript`] the
//! test holds. No streamed reply exists until the test releases it, and a
//! request stream the method leaves open is read by a retained reader that
//! records the status tonic hands it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio_stream::StreamExt;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::{Code, Request, Response, Status, Streaming};

/// The generated bindings, including the empty wrapper module camber-build
/// leaves for a streaming service.
#[allow(dead_code)]
pub mod proto {
    tonic::include_proto!("greeter");
}

pub use proto::native_forms_client::NativeFormsClient;
pub use proto::native_forms_server::NativeFormsServer;
pub use proto::{HelloReply, HelloRequest};

/// The request metadata naming which form a call drives.
pub const REQUEST_KEY: &str = "x-form-request";

/// The response metadata that echoes [`REQUEST_KEY`] back on the head.
pub const ECHO_KEY: &str = "x-form-echo";

/// The status metadata naming which form refused.
pub const TRAILER_KEY: &str = "x-form-trailer";

/// The request name every form refuses.
pub const FAIL_NAME: &str = "fail";

/// The code every form refuses with.
pub const FAIL_CODE: Code = Code::FailedPrecondition;

/// The message every form refuses with.
pub const FAIL_MESSAGE: &str = "fixture refusal";

/// One native tonic RPC form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Form {
    Unary,
    ClientStreaming,
    ServerStreaming,
    Bidirectional,
}

impl Form {
    /// Every form, in declaration order.
    pub const ALL: [Self; 4] = [
        Self::Unary,
        Self::ClientStreaming,
        Self::ServerStreaming,
        Self::Bidirectional,
    ];

    /// The name a row and its metadata carry for this form.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Unary => "unary",
            Self::ClientStreaming => "client-streaming",
            Self::ServerStreaming => "server-streaming",
            Self::Bidirectional => "bidirectional",
        }
    }

    /// The wire path tonic dispatches this form's method on.
    pub const fn path(self) -> &'static str {
        match self {
            Self::Unary => "/greeter.NativeForms/Unary",
            Self::ClientStreaming => "/greeter.NativeForms/ClientStreaming",
            Self::ServerStreaming => "/greeter.NativeForms/ServerStreaming",
            Self::Bidirectional => "/greeter.NativeForms/Bidirectional",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Unary => 0,
            Self::ClientStreaming => 1,
            Self::ServerStreaming => 2,
            Self::Bidirectional => 3,
        }
    }
}

/// How many times each form's method was entered.
#[derive(Default)]
pub struct FormEntries {
    counts: [AtomicUsize; 4],
}

impl FormEntries {
    /// How many times `form`'s method has been entered.
    pub fn entered(&self, form: Form) -> usize {
        self.counts[form.index()].load(Ordering::SeqCst)
    }

    fn enter(&self, form: Form) {
        self.counts[form.index()].fetch_add(1, Ordering::SeqCst);
    }
}

/// The fixed service's server-streaming answer: a greeting, then a farewell.
pub type FixedReplies = tokio_stream::Iter<std::array::IntoIter<Result<HelloReply, Status>, 2>>;

/// The fixed service's bidirectional answer: one greeting per request message.
pub type GreetedReplies = tokio_stream::adapters::Map<
    Streaming<HelloRequest>,
    fn(Result<HelloRequest, Status>) -> Result<HelloReply, Status>,
>;

/// The scripted service's streamed answer: whatever the test feeds it.
pub type FedReplies = UnboundedReceiverStream<ScriptedReply>;

/// The fixture service, counting into entries the test holds.
pub struct FormsService {
    entries: Arc<FormEntries>,
}

impl FormsService {
    /// Serve every form, counting entries into `entries`.
    pub fn serve(entries: &Arc<FormEntries>) -> NativeFormsServer<Self> {
        NativeFormsServer::new(Self {
            entries: Arc::clone(entries),
        })
    }
}

#[tonic::async_trait]
impl proto::native_forms_server::NativeForms for FormsService {
    type ServerStreamingStream = FixedReplies;
    type BidirectionalStream = GreetedReplies;

    async fn unary(&self, request: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
        self.entries.enter(Form::Unary);
        let echo = echo_of(request.metadata());
        let reply = greeting(Form::Unary, &request.into_inner().name)?;
        Ok(echoed(echo, reply))
    }

    async fn client_streaming(
        &self,
        request: Request<Streaming<HelloRequest>>,
    ) -> Result<Response<HelloReply>, Status> {
        self.entries.enter(Form::ClientStreaming);
        let echo = echo_of(request.metadata());
        let mut incoming = request.into_inner();
        let mut names = Vec::new();
        while let Some(message) = incoming.message().await? {
            names.push(refused_or(Form::ClientStreaming, message.name)?);
        }
        Ok(echoed(echo, greeted_all(&names)))
    }

    async fn server_streaming(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<FixedReplies>, Status> {
        self.entries.enter(Form::ServerStreaming);
        let echo = echo_of(request.metadata());
        let name = request.into_inner().name;
        let opening = Ok(HelloReply {
            message: greeting_text(&name),
        });
        let closing = refused_or(Form::ServerStreaming, name.as_str()).map(|name| HelloReply {
            message: format!("Goodbye, {name}!"),
        });
        Ok(echoed(echo, tokio_stream::iter([opening, closing])))
    }

    async fn bidirectional(
        &self,
        request: Request<Streaming<HelloRequest>>,
    ) -> Result<Response<GreetedReplies>, Status> {
        self.entries.enter(Form::Bidirectional);
        let echo = echo_of(request.metadata());
        let greet: fn(Result<HelloRequest, Status>) -> Result<HelloReply, Status> =
            |message| message.and_then(|message| greeting(Form::Bidirectional, &message.name));
        Ok(echoed(echo, request.into_inner().map(greet)))
    }
}

/// One item a scripted call's response stream carries.
pub type ScriptedReply = Result<HelloReply, Status>;

/// When a scripted client-streaming call answers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientAnswer {
    /// After it has read every request message.
    AfterUpload,
    /// After its first request message. A retained reader keeps reading the
    /// rest, so the request stream outlives the committed head.
    Early,
    /// [`Self::Early`], with the retained reader parked until
    /// [`FormScript::start_reader`]. Until then nothing polls the request
    /// stream, so a hold armed on a transfer edge can only be taken by the
    /// response.
    EarlyParked,
}

/// The controls one scripted call answers to.
///
/// One call per script: the response stream is handed to the first streaming
/// method that asks for it. The test feeds that stream through the
/// [`ReplyFeed`] [`FormScript::new`] returns, so no response message exists
/// until the test releases it. Every method counts its entry into the same
/// [`FormEntries`] the fixed service uses.
pub struct FormScript {
    entries: FormEntries,
    client_answer: ClientAnswer,
    response: Mutex<Option<ResponseHalves>>,
    /// The status tonic handed a method for its failed request stream.
    failure: Mutex<Option<(Code, Box<str>)>>,
    /// The reader a call left running past its own return, if any.
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// What a parked reader waits on before it reads on.
    reader_start: tokio::sync::Notify,
}

/// The test's half of one scripted response stream.
///
/// Dropped or [`ended`](Self::end), it ends the stream once no reader still
/// holds a sender.
pub struct ReplyFeed {
    sender: Option<tokio::sync::mpsc::UnboundedSender<ScriptedReply>>,
}

impl ReplyFeed {
    /// Release one response message carrying `message`.
    ///
    /// `false` when the call already dropped its response stream.
    pub fn reply(&self, message: &str) -> bool {
        self.sender.as_ref().is_some_and(|sender| {
            sender
                .send(Ok(HelloReply {
                    message: message.into(),
                }))
                .is_ok()
        })
    }

    /// Stop feeding the response stream.
    pub fn end(&mut self) {
        self.sender = None;
    }
}

impl FormScript {
    /// One script, and the feed its response stream reads from.
    pub fn new(client_answer: ClientAnswer) -> (Arc<Self>, ReplyFeed) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let script = Arc::new(Self {
            entries: FormEntries::default(),
            client_answer,
            response: Mutex::new(Some((receiver, sender.clone()))),
            failure: Mutex::new(None),
            reader: Mutex::new(None),
            reader_start: tokio::sync::Notify::new(),
        });
        (
            script,
            ReplyFeed {
                sender: Some(sender),
            },
        )
    }

    /// How many times `form`'s method has been entered.
    pub fn entered(&self, form: Form) -> usize {
        self.entries.entered(form)
    }

    /// The code and message of the status tonic derived from a failed request
    /// stream, as the method that read it was handed it.
    pub fn upload_failure(&self) -> Option<(Code, Box<str>)> {
        locked(&self.failure).clone()
    }

    /// Let a reader parked under [`ClientAnswer::EarlyParked`] read on.
    ///
    /// Remembered when no reader waits yet, so the order of the two cannot
    /// strand one.
    pub fn start_reader(&self) {
        self.reader_start.notify_one();
    }

    /// Join the reader a call left running, under `bound`.
    ///
    /// # Errors
    ///
    /// When the reader panicked, or outlived `bound`; it is aborted first.
    pub async fn join_reader(&self, bound: std::time::Duration) -> Result<(), String> {
        let reader = locked(&self.reader).take();
        let Some(reader) = reader else {
            return Ok(());
        };
        let abort = reader.abort_handle();
        match tokio::time::timeout(bound, reader).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("the retained request reader failed: {error}")),
            Err(_) => {
                abort.abort();
                Err("the retained request reader outlived its bound".into())
            }
        }
    }

    /// Record the status a method was handed for its failed request stream.
    fn record_failure(&self, status: &Status) {
        *locked(&self.failure) = Some((status.code(), status.message().into()));
    }

    /// Take this script's response stream, and the sender beside it.
    fn take_response(&self) -> Result<ResponseHalves, Status> {
        locked(&self.response)
            .take()
            .ok_or_else(|| Status::failed_precondition("one scripted call per script"))
    }

    /// Keep reading `incoming` past the method's return, first waiting for
    /// [`Self::start_reader`] when `parked`.
    fn retain(
        self: &Arc<Self>,
        incoming: Streaming<HelloRequest>,
        sink: Option<ReplySink>,
        parked: bool,
    ) {
        let script = Arc::clone(self);
        let reader = tokio::spawn(async move {
            if parked {
                script.reader_start.notified().await;
            }
            script.read_out(incoming, sink).await;
        });
        *locked(&self.reader) = Some(reader);
    }

    /// Read `incoming` to its end, recording and forwarding a failure.
    async fn read_out(&self, mut incoming: Streaming<HelloRequest>, sink: Option<ReplySink>) {
        let failure = loop {
            match incoming.message().await {
                Ok(Some(_message)) => {}
                Ok(None) => return,
                Err(status) => break status,
            }
        };
        self.record_failure(&failure);
        // A response stream the call already dropped has no peer left to tell.
        if let Some(sink) = sink {
            let _ = sink.send(Err(failure));
        }
    }
}

type ReplySink = tokio::sync::mpsc::UnboundedSender<ScriptedReply>;

/// One script control, read through a poisoned lock: a panicked call leaves
/// the value it held, and the row still reads it.
fn locked<T>(control: &Mutex<T>) -> MutexGuard<'_, T> {
    control.lock().unwrap_or_else(|error| error.into_inner())
}

/// A scripted response stream's receiver, and the sender a bidirectional
/// call's reader forwards its request-stream failure through.
type ResponseHalves = (
    tokio::sync::mpsc::UnboundedReceiver<ScriptedReply>,
    ReplySink,
);

/// The scripted service, answering every form under one [`FormScript`].
pub struct ScriptedForms {
    script: Arc<FormScript>,
}

impl ScriptedForms {
    /// Serve every form under `script`.
    pub fn serve(script: &Arc<FormScript>) -> NativeFormsServer<Self> {
        NativeFormsServer::new(Self {
            script: Arc::clone(script),
        })
    }
}

/// The reply text one greeted name produces.
pub fn greeting_text(name: &str) -> String {
    format!("Hello, {name}!")
}

#[tonic::async_trait]
impl proto::native_forms_server::NativeForms for ScriptedForms {
    type ServerStreamingStream = FedReplies;
    type BidirectionalStream = FedReplies;

    async fn unary(&self, request: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
        self.script.entries.enter(Form::Unary);
        Ok(Response::new(HelloReply {
            message: greeting_text(&request.into_inner().name),
        }))
    }

    async fn client_streaming(
        &self,
        request: Request<Streaming<HelloRequest>>,
    ) -> Result<Response<HelloReply>, Status> {
        self.script.entries.enter(Form::ClientStreaming);
        let mut incoming = request.into_inner();
        let mut names = Vec::new();
        loop {
            let message = incoming.message().await.inspect_err(|status| {
                self.script.record_failure(status);
            })?;
            match (message, self.script.client_answer) {
                (Some(message), answer @ (ClientAnswer::Early | ClientAnswer::EarlyParked)) => {
                    self.script
                        .retain(incoming, None, answer == ClientAnswer::EarlyParked);
                    return Ok(Response::new(HelloReply {
                        message: greeting_text(&message.name),
                    }));
                }
                (Some(message), ClientAnswer::AfterUpload) => names.push(message.name),
                (None, _) => break,
            }
        }
        Ok(Response::new(greeted_all(&names)))
    }

    async fn server_streaming(
        &self,
        _request: Request<HelloRequest>,
    ) -> Result<Response<FedReplies>, Status> {
        self.script.entries.enter(Form::ServerStreaming);
        // Only the test feeds this stream, so the sender beside it goes.
        let (replies, _sink) = self.script.take_response()?;
        Ok(Response::new(FedReplies::new(replies)))
    }

    async fn bidirectional(
        &self,
        request: Request<Streaming<HelloRequest>>,
    ) -> Result<Response<FedReplies>, Status> {
        self.script.entries.enter(Form::Bidirectional);
        let (replies, sink) = self.script.take_response()?;
        self.script.retain(request.into_inner(), Some(sink), false);
        Ok(Response::new(FedReplies::new(replies)))
    }
}

/// One encoded gRPC response frame carrying `message`.
///
/// What the download owner counts for one reply: tonic writes each message as
/// one length-prefixed frame, and trailers carry no payload bytes.
pub fn reply_frame(message: &str) -> Box<[u8]> {
    grpc_frame(&prost::Message::encode_to_vec(&HelloReply {
        message: message.into(),
    }))
}

/// The form metadata a request carried, if any.
fn echo_of(metadata: &MetadataMap) -> Option<MetadataValue<tonic::metadata::Ascii>> {
    metadata.get(REQUEST_KEY).cloned()
}

/// Answer `message`, with the request's form metadata echoed on the head.
fn echoed<T>(echo: Option<MetadataValue<tonic::metadata::Ascii>>, message: T) -> Response<T> {
    let mut response = Response::new(message);
    if let Some(value) = echo {
        response.metadata_mut().insert(ECHO_KEY, value);
    }
    response
}

/// The one reply a client-streaming call answers every name it read with.
fn greeted_all(names: &[String]) -> HelloReply {
    HelloReply {
        message: greeting_text(&names.join(",")),
    }
}

/// Greet `name`, or refuse it as `form` when it is [`FAIL_NAME`].
fn greeting(form: Form, name: &str) -> Result<HelloReply, Status> {
    refused_or(form, name).map(|name| HelloReply {
        message: greeting_text(name),
    })
}

/// Pass `name` through, or refuse it as `form` when it is [`FAIL_NAME`].
fn refused_or<N: AsRef<str>>(form: Form, name: N) -> Result<N, Status> {
    match name.as_ref() {
        FAIL_NAME => Err(refusal(form)),
        _ => Ok(name),
    }
}

/// The typed status `form` refuses with, its metadata naming the form.
fn refusal(form: Form) -> Status {
    let mut metadata = MetadataMap::new();
    metadata.insert(TRAILER_KEY, MetadataValue::from_static(form.name()));
    Status::with_metadata(FAIL_CODE, FAIL_MESSAGE, metadata)
}

/// One length-prefixed, uncompressed gRPC frame around `payload`.
///
/// For the rows that frame a call by hand: a generated client sends the whole
/// call and reads the whole answer, which no row that holds a frame back can
/// use.
pub fn grpc_frame(payload: &[u8]) -> Box<[u8]> {
    let declared = u32::try_from(payload.len()).expect("a fixture payload fits one frame");
    let mut framed = Vec::with_capacity(payload.len() + 5);
    framed.push(0);
    framed.extend_from_slice(&declared.to_be_bytes());
    framed.extend_from_slice(payload);
    framed.into_boxed_slice()
}

/// One gRPC frame carrying a `HelloRequest` named `name`.
pub fn hello_frame(name: &str) -> Box<[u8]> {
    grpc_frame(&prost::Message::encode_to_vec(&HelloRequest {
        name: name.into(),
    }))
}

/// The head every hand-framed gRPC call opens its stream with.
pub const GRPC_HEADERS: [(&str, &str); 2] =
    [("content-type", "application/grpc"), ("te", "trailers")];
