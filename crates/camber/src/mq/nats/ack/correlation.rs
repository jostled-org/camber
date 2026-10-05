//! One connection's acknowledgement state: the stream every receipt must
//! name, and one correlation entry per admitted acknowledged publish.
//!
//! An entry holds only the sender its publish waits on. Its registration
//! removes it on every exit, and routing a reply removes it first, so a
//! duplicate or late reply finds nothing. Tokens are never reused, so a
//! retired token cannot collide with a live one. The map grows with peak
//! concurrent publishes, which admission bounds, never with their count.

use super::decode::acknowledgement;
use super::identity::ReplyTokens;
use crate::IntegrationError;
use crate::mq::nats::failure::{acknowledgement_inconclusive, no_stream_reached, tokens_exhausted};
use crate::runtime_state::recover_poisoned;
use crate::runtime_test_support::{NatsAckReceiver, NatsAckSnapshot};
use async_nats::header::NATS_EXPECTED_STREAM;
use async_nats::{HeaderMap, Message, Subject};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::oneshot;

/// The acknowledgement state one acknowledged connection shares between its
/// publishes and its reply receiver.
pub(crate) struct Acknowledgements {
    /// The private inbox every reply subject extends with one token.
    inbox: Box<str>,
    /// The stream a receipt must name.
    stream: Box<str>,
    /// The expected-stream header every publication carries.
    headers: HeaderMap,
    pending: Mutex<Pending>,
}

/// What the lock guards. No await, decode, or network work runs under it.
struct Pending {
    tokens: ReplyTokens,
    waiting: HashMap<u64, oneshot::Sender<Message>>,
    /// Whether the receiver stopped routing; nothing registers after.
    ended: bool,
}

impl Acknowledgements {
    /// The state of a connection whose replies arrive under `inbox` and
    /// whose receipts must name `stream`.
    pub(super) fn new(inbox: Box<str>, stream: Box<str>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(NATS_EXPECTED_STREAM, &*stream);
        Self {
            inbox,
            stream,
            headers,
            pending: Mutex::new(Pending {
                tokens: ReplyTokens::starting_at(0),
                waiting: HashMap::new(),
                ended: false,
            }),
        }
    }

    /// The one wildcard subject every reply of this connection matches.
    pub(super) fn wildcard(&self) -> String {
        format!("{}.*", self.inbox)
    }

    /// Register one admitted publish before it is submitted.
    ///
    /// # Errors
    ///
    /// `Unavailable/Safe` once the receiver stopped, and
    /// `LimitExceeded/Never` once the connection's tokens are spent. Both
    /// refuse before anything is sent.
    pub(crate) fn register(self: &Arc<Self>) -> Result<Registration, IntegrationError> {
        let (sender, reply) = oneshot::channel();
        let token = {
            let mut pending = self.lock();
            let token = match pending.ended {
                true => Err(no_stream_reached()),
                false => pending.tokens.issue().map_err(tokens_exhausted),
            }?;
            pending.waiting.insert(token, sender);
            token
        };
        Ok(Registration {
            token,
            owner: Arc::clone(self),
            reply,
        })
    }

    /// Hand `reply` to the publish its token names, and discard a reply no
    /// live publish holds before anything decodes it.
    pub(super) fn route(&self, reply: Message) {
        let Some(token) = self.token_of(&reply.subject) else {
            return;
        };
        let waiting = self.lock().waiting.remove(&token);
        if let Some(sender) = waiting {
            // A publish that stopped waiting already retired its own entry.
            drop(sender.send(reply));
        }
    }

    /// Stop routing: every waiting publish loses its answer, and nothing
    /// registers again.
    pub(super) fn end(&self) {
        let mut pending = self.lock();
        pending.ended = true;
        pending.waiting = HashMap::new();
    }

    /// The live entries, the map's allocation, and whether the receiver
    /// still routes.
    pub(crate) fn snapshot(&self) -> NatsAckSnapshot {
        let pending = self.lock();
        NatsAckSnapshot {
            pending: pending.waiting.len(),
            capacity: pending.waiting.capacity(),
            receiver: match pending.ended {
                true => NatsAckReceiver::Ended,
                false => NatsAckReceiver::Routing,
            },
        }
    }

    /// The token a reply subject names under this connection's inbox.
    fn token_of(&self, subject: &Subject) -> Option<u64> {
        let token = subject
            .as_str()
            .strip_prefix(&*self.inbox)?
            .strip_prefix('.')?;
        match !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()) {
            true => token.parse().ok(),
            false => None,
        }
    }

    fn retire(&self, token: u64) {
        self.lock().waiting.remove(&token);
    }

    fn lock(&self) -> MutexGuard<'_, Pending> {
        recover_poisoned(self.pending.lock())
    }
}

/// One admitted publish's correlation entry, removed when this drops.
pub(crate) struct Registration {
    token: u64,
    owner: Arc<Acknowledgements>,
    reply: oneshot::Receiver<Message>,
}

impl Registration {
    /// The reply subject the publication names.
    pub(crate) fn reply_subject(&self) -> Subject {
        Subject::from(format!("{}.{}", self.owner.inbox, self.token))
    }

    /// The expected-stream header the publication carries.
    pub(crate) fn headers(&self) -> HeaderMap {
        self.owner.headers.clone()
    }

    /// Wait for the reply routed to this publish.
    ///
    /// # Errors
    ///
    /// `OutcomeUnknown` when the receiver stopped first: the server may have
    /// stored the message.
    pub(crate) async fn reply(&mut self) -> Result<Message, IntegrationError> {
        (&mut self.reply)
            .await
            .map_err(|_| acknowledgement_inconclusive())
    }

    /// Settle the publish by `reply`.
    ///
    /// # Errors
    ///
    /// The refusal or uncertainty the reply carries.
    pub(crate) fn accept(&self, reply: &Message) -> Result<(), IntegrationError> {
        acknowledgement(reply, &self.owner.stream)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.owner.retire(self.token);
    }
}
