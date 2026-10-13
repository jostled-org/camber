//! The source one streamed body is read from, and what a watched peer's
//! streams did.
//!
//! A source is the transport's body or a mock's canned bytes. The body engine
//! reads both the same way.
//!
//! The counts are read-only. A test that watches a peer reads what the
//! production source and its owner task did. With no watcher, a stream keeps
//! no record and counts nothing.

use super::super::body::{BodyWatch, ClientBodyObservation, SourceCounts};
use crate::runtime_state::recover_poisoned;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

/// What one stream's source and its owner task did.
#[derive(Default)]
pub(super) struct StreamRecord {
    polls: AtomicUsize,
    frames: AtomicUsize,
    released: AtomicBool,
    owner_ended: AtomicBool,
    /// The counts of the stream's body, once its owner exists.
    body: OnceLock<BodyWatch>,
}

impl StreamRecord {
    /// Read the body counts of this stream through `watch`.
    pub(super) fn watch_body(&self, watch: BodyWatch) {
        self.body.get_or_init(|| watch);
    }

    fn observed(&self) -> ClientBodyObservation {
        let source = SourceCounts {
            polls: self.polls.load(Ordering::Acquire),
            frames: self.frames.load(Ordering::Acquire),
            released: self.released.load(Ordering::Acquire),
            owner_ended: self.owner_ended.load(Ordering::Acquire),
        };
        ClientBodyObservation::of(source, self.body.get())
    }
}

/// What the streams of the calls to one watched peer did.
///
/// It holds the record of the latest stream. Each stream writes only its own
/// record, so one stream's end is never read as another's.
#[derive(Default)]
pub(in crate::http) struct StreamMetrics {
    latest: Mutex<Option<Arc<StreamRecord>>>,
}

impl StreamMetrics {
    /// Lock the latest record. No holder can panic with it half-written.
    fn latest(&self) -> std::sync::MutexGuard<'_, Option<Arc<StreamRecord>>> {
        recover_poisoned(self.latest.lock())
    }

    /// Begin the record of one stream, and make it the latest.
    pub(super) fn open(&self) -> Arc<StreamRecord> {
        let record = Arc::new(StreamRecord::default());
        *self.latest() = Some(Arc::clone(&record));
        record
    }

    /// A copy of the latest stream's counts. No stream yet is all zeros.
    pub(in crate::http) fn observed(&self) -> ClientBodyObservation {
        self.latest()
            .as_ref()
            .map(|record| record.observed())
            .unwrap_or_default()
    }
}

/// The witness that a stream's owner task has ended.
///
/// The task holds it to its last statement. Its drop is the record, so an
/// aborted task records the end too.
pub(super) struct OwnerEnded(Option<Arc<StreamRecord>>);

impl OwnerEnded {
    pub(super) fn of(record: Option<Arc<StreamRecord>>) -> Self {
        Self(record)
    }
}

impl Drop for OwnerEnded {
    fn drop(&mut self) {
        if let Some(record) = &self.0 {
            record.owner_ended.store(true, Ordering::Release);
        }
    }
}

/// The failed read of a source. It holds nothing: the engine names the
/// category, and the transport's own text is dropped with its error.
pub(super) struct SourceFailure;

/// What one poll of a source returns.
type Polled = Poll<Option<Result<Frame<Bytes>, SourceFailure>>>;

/// One poll of a body, with its failure reduced to [`SourceFailure`].
fn polled<E>(read: Poll<Option<Result<Frame<Bytes>, E>>>) -> Polled {
    read.map(|frame| frame.map(|read| read.map_err(|_| SourceFailure)))
}

/// The body of a transport's answer, read frame by frame.
///
/// The drop is the release: it closes a connection whose body is unread.
pub(super) struct Wire {
    body: reqwest::Body,
    record: Option<Arc<StreamRecord>>,
}

impl Wire {
    fn poll(&mut self, context: &mut Context<'_>) -> Polled {
        if let Some(record) = &self.record {
            record.polls.fetch_add(1, Ordering::Release);
        }
        let read = Pin::new(&mut self.body).poll_frame(context);
        if let (Some(record), Poll::Ready(_)) = (&self.record, &read) {
            record.frames.fetch_add(1, Ordering::Release);
        }
        polled(read)
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        if let Some(record) = &self.record {
            record.released.store(true, Ordering::Release);
        }
    }
}

/// The source of one streamed body.
pub(super) enum Source {
    /// The unread body of a transport's answer.
    Wire(Wire),
    /// The whole body of a mock's answer. It is one frame, or none when the
    /// body is empty.
    Canned(Full<Bytes>),
}

impl Source {
    /// The source of a transport's `response`, counted on `record`.
    pub(super) fn wire(response: reqwest::Response, record: Option<Arc<StreamRecord>>) -> Self {
        Self::Wire(Wire {
            body: reqwest::Body::from(response),
            record,
        })
    }

    /// The source of a mock answer's `body`.
    pub(super) fn canned(body: Bytes) -> Self {
        Self::Canned(Full::new(body))
    }
}

impl Body for Source {
    type Data = Bytes;
    type Error = SourceFailure;

    fn poll_frame(self: Pin<&mut Self>, context: &mut Context<'_>) -> Polled {
        match self.get_mut() {
            Self::Wire(wire) => wire.poll(context),
            Self::Canned(whole) => polled(Pin::new(whole).poll_frame(context)),
        }
    }
}
