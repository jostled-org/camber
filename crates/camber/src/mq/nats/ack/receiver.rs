//! The one private reply subscription of an acknowledged connection.
//!
//! The receiver routes replies; it never settles a publish, reports a
//! terminal, or holds access to its connection. It runs inside the
//! connection's close owner and routes until that owner's close begins, which
//! is after every admitted operation finished. Ending it, or dropping it with
//! a forced stop, ends routing, so a publish still waiting reads an unknown
//! outcome rather than waiting out its deadline.

use super::correlation::Acknowledgements;
use crate::IntegrationError;
use crate::mq::nats::failure::reply_subscription_refused;
use async_nats::{Client, Subscriber};
use futures_util::StreamExt;
use std::future::Future;
use std::sync::Arc;

/// The private wildcard inbox subscription and the state it routes into.
pub(crate) struct ReplyReceiver {
    subscriber: Subscriber,
    acknowledgements: Arc<Acknowledgements>,
}

impl ReplyReceiver {
    /// Subscribe `client` to a fresh private inbox for receipts that must
    /// name `stream`.
    ///
    /// The subscription is queued here; the caller's readiness flush sends
    /// it. It takes no application subscription slot.
    ///
    /// # Errors
    ///
    /// `Connect/Unavailable/Safe` when the SDK refuses the subscription.
    pub(crate) async fn install(
        client: &Client,
        stream: Box<str>,
    ) -> Result<Self, IntegrationError> {
        let acknowledgements = Arc::new(Acknowledgements::new(client.new_inbox().into(), stream));
        let subscriber = client
            .subscribe(acknowledgements.wildcard())
            .await
            .map_err(reply_subscription_refused)?;
        Ok(Self {
            subscriber,
            acknowledgements,
        })
    }

    /// The state this receiver routes into, for the connection's publishes.
    pub(crate) fn acknowledgements(&self) -> Arc<Acknowledgements> {
        Arc::clone(&self.acknowledgements)
    }

    /// Route replies until `stop` resolves, then end routing.
    ///
    /// A subscription the SDK ends first ends routing at once; the receiver
    /// still waits for `stop`, so its owner's order is unchanged.
    pub(crate) async fn serve_until(mut self, stop: impl Future<Output = ()>) {
        let mut stop = std::pin::pin!(stop);
        let ended = tokio::select! {
            biased;
            () = stop.as_mut() => false,
            () = route_replies(&mut self.subscriber, &self.acknowledgements) => true,
        };
        if ended {
            stop.await;
        }
    }
}

impl Drop for ReplyReceiver {
    fn drop(&mut self) {
        self.acknowledgements.end();
    }
}

/// Route every reply `subscriber` delivers, and end routing once it ends.
async fn route_replies(subscriber: &mut Subscriber, acknowledgements: &Acknowledgements) {
    while let Some(reply) = subscriber.next().await {
        acknowledgements.route(reply);
    }
    acknowledgements.end();
}
