//! Observe SDK queue admission while holding publishes before their flush.

use super::nats_publish::wait_released;
use crate::mq::nats::Connection;
use std::future::{Future, poll_fn};
use std::sync::Arc;
use tokio::sync::watch;

/// A scheduling checkpoint around the real SDK publish queue.
/// It records actual polls and admissions; it does not select their results.
#[doc(hidden)]
pub struct NatsQueueProbe {
    observed: watch::Receiver<(usize, usize)>,
    release: watch::Sender<bool>,
}

impl NatsQueueProbe {
    /// Attach before cloning the connection. Hold admitted publishes before flush.
    #[must_use]
    pub fn hold(connection: &mut Connection) -> Option<Self> {
        let (observations, observed) = watch::channel((0, 0));
        let (release, released) = watch::channel(false);
        connection.hold_queue(Arc::new(QueueCheckpoint {
            observations,
            released,
        }))?;
        Some(Self { observed, release })
    }

    /// Wait for `count` distinct submission futures to be polled, then read admissions.
    /// Returns `None` if the connection ended before `count` were polled.
    pub async fn polled(&mut self, count: usize) -> Option<usize> {
        self.observed
            .wait_for(|(polled, _)| *polled >= count)
            .await
            .ok()
            .map(|state| state.1)
    }

    /// Allow admitted publishes to flush. Dropping the probe also releases them.
    pub fn release(self) {
        self.release.send_replace(true);
    }
}

/// Production-side observations of the SDK future and a pre-flush rendezvous.
pub(crate) struct QueueCheckpoint {
    observations: watch::Sender<(usize, usize)>,
    released: watch::Receiver<bool>,
}

impl QueueCheckpoint {
    pub(crate) async fn observe<F, T, E>(&self, queued: F) -> Result<T, E>
    where
        F: Future<Output = Result<T, E>>,
    {
        let mut queued = std::pin::pin!(queued);
        let mut first = true;
        poll_fn(|cx| {
            let result = queued.as_mut().poll(cx);
            let newly_polled = std::mem::take(&mut first);
            self.observations.send_modify(|(polled, admitted)| {
                *polled += usize::from(newly_polled);
                *admitted += usize::from(matches!(&result, std::task::Poll::Ready(Ok(_))));
            });
            result
        })
        .await
    }

    pub(crate) async fn before_flush(&self) {
        wait_released(&self.released).await;
    }
}
