//! Test-only JetStream streams for the local NATS lane.
//!
//! [`StreamOwner`] is the one owner of every stream a selected test creates.
//! It runs its own SDK client on a private Tokio runtime, outside every Camber
//! runtime, so no production Camber code administers a stream. It records each
//! name before the server can create it, so an ambiguous create is still
//! deleted. Release deletes exactly the recorded names, proves the server no
//! longer knows them, then drains the client and proves it closed. Release
//! runs from [`StreamOwner::finish`] on every verdict, and from `Drop` after a
//! panic. The run's container removal stays the fallback, not the witness.

use crate::integration_rows::{ROW_BOUND, Row};

use async_nats::jetstream::{self, ErrorCode, context::GetStreamErrorKind, stream};
use std::future::Future;
use std::time::Duration;

/// How often release polls the drained client for closure.
const CLOSE_POLL: Duration = Duration::from_millis(10);

/// One message a stream stored: its subject and bytes.
pub type Stored = (Box<str>, Box<[u8]>);

/// The owner of a test's JetStream streams and the SDK client that manages them.
pub struct StreamOwner {
    tokio: tokio::runtime::Runtime,
    client: async_nats::Client,
    jetstream: jetstream::Context,
    owned: Vec<Box<str>>,
    released: bool,
}

impl StreamOwner {
    /// Connect the owner's own SDK client to `url`.
    ///
    /// # Errors
    ///
    /// When the private runtime cannot start or the client cannot connect
    /// within [`ROW_BOUND`].
    pub fn connect(url: &str) -> Result<Self, String> {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| format!("stream owner runtime: {error}"))?;
        let client = within(&tokio, "the stream owner connect", async_nats::connect(url))?
            .map_err(|error| format!("stream owner connect: {error}"))?;
        // The context spawns its acknowledgement task on the owner's runtime.
        let jetstream = {
            let _entered = tokio.enter();
            jetstream::new(client.clone())
        };
        Ok(Self {
            tokio,
            client,
            jetstream,
            owned: Vec::new(),
            released: false,
        })
    }

    /// Create the stream `name` capturing only `subject`, with file storage
    /// inside the server's container.
    ///
    /// # Errors
    ///
    /// When the server refuses the stream or does not answer in time.
    pub fn create(&mut self, name: &str, subject: &str) -> Row {
        self.owned.push(name.into());
        let config = stream::Config {
            name: name.to_owned(),
            subjects: vec![subject.to_owned()],
            storage: stream::StorageType::File,
            ..stream::Config::default()
        };
        within(
            &self.tokio,
            "the stream create",
            self.jetstream.create_stream(config),
        )?
        .map(drop)
        .map_err(|error| format!("create stream {name}: {error}"))
    }

    /// Every message the stream `name` holds, in sequence order, read back
    /// through the owner's own client.
    ///
    /// # Errors
    ///
    /// When the stream or one of its messages cannot be read in time.
    pub fn stored(&self, name: &str) -> Result<Box<[Stored]>, String> {
        within(&self.tokio, "the stream readback", async {
            let mut stream = self
                .jetstream
                .get_stream(name)
                .await
                .map_err(|error| format!("look up stream {name}: {error}"))?;
            let state = stream
                .info()
                .await
                .map_err(|error| format!("stream {name} info: {error}"))?
                .state
                .clone();
            let mut stored = Vec::new();
            if state.messages > 0 {
                for sequence in state.first_sequence..=state.last_sequence {
                    let message = stream
                        .get_raw_message(sequence)
                        .await
                        .map_err(|error| format!("read {name} message {sequence}: {error}"))?;
                    stored.push((
                        message.subject.as_str().into(),
                        message.payload.as_ref().into(),
                    ));
                }
            }
            Ok(stored.into_boxed_slice())
        })?
    }

    /// Release every owned stream and the client, preserving each failure.
    ///
    /// # Errors
    ///
    /// Every stream that survived deletion and any client that stayed open.
    pub fn finish(mut self) -> Row {
        self.release()
    }

    /// Delete each owned stream, prove each absent, then close the client.
    /// Runs once; a later call answers `Ok`.
    fn release(&mut self) -> Row {
        if self.released {
            return Ok(());
        }
        self.released = true;
        let mut failures: Vec<String> = std::mem::take(&mut self.owned)
            .iter()
            .filter_map(|name| self.delete(name).err())
            .collect();
        failures.extend(self.close().err());
        match failures.is_empty() {
            true => Ok(()),
            false => Err(failures.join("; ")),
        }
    }

    /// Require the server to report no stream named `name`.
    ///
    /// # Errors
    ///
    /// When the stream exists, or the server's answer proves nothing.
    pub fn absent(&self, name: &str) -> Row {
        within(&self.tokio, "the stream lookup", self.lookup_absent(name))?
    }

    /// Delete the stream `name`, then require the server to report it absent.
    fn delete(&self, name: &str) -> Row {
        within(&self.tokio, "the stream delete", async {
            // A create the server never applied leaves nothing to delete, so
            // the delete answer decides nothing; the absence check does. A
            // failed absence check still reports what the delete answered.
            let deleted = self.jetstream.delete_stream(name).await;
            self.lookup_absent(name)
                .await
                .map_err(|absence| match deleted {
                    Ok(_) => absence,
                    Err(error) => format!("{absence}; the delete of stream {name} failed: {error}"),
                })
        })?
    }

    async fn lookup_absent(&self, name: &str) -> Row {
        match self.jetstream.get_stream(name).await {
            Ok(_) => Err(format!("stream {name} exists")),
            Err(error) if stream_not_found(&error) => Ok(()),
            Err(error) => Err(format!("cannot prove stream {name} is absent: {error}")),
        }
    }

    /// Drain the client, then wait until it refuses a flush: its connection
    /// handler has exited.
    fn close(&self) -> Row {
        within(&self.tokio, "the stream owner close", async {
            self.client
                .drain()
                .await
                .map_err(|error| format!("drain the stream owner client: {error}"))?;
            while self.client.flush().await.is_ok() {
                tokio::time::sleep(CLOSE_POLL).await;
            }
            Ok(())
        })?
    }
}

impl Drop for StreamOwner {
    fn drop(&mut self) {
        // A panic skipped `finish`; the run's container removal reports any
        // residue this cannot release.
        let _ignored = self.release();
    }
}

/// Whether the server answered that no such stream exists.
fn stream_not_found(error: &jetstream::context::GetStreamError) -> bool {
    matches!(
        error.kind(),
        GetStreamErrorKind::JetStream(source) if source.error_code() == ErrorCode::STREAM_NOT_FOUND
    )
}

/// Drive `future` on the owner's runtime under [`ROW_BOUND`].
fn within<F: Future>(
    tokio: &tokio::runtime::Runtime,
    what: &str,
    future: F,
) -> Result<F::Output, String> {
    // The guard's timer must register inside the runtime it waits on.
    tokio
        .block_on(async { tokio::time::timeout(ROW_BOUND, future).await })
        .map_err(|_| format!("{what} did not finish within {ROW_BOUND:?}"))
}
