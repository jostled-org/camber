//! A per-connection scheduling checkpoint before NATS publish submission.

use crate::mq::nats::Connection;
use std::sync::Arc;
use tokio::sync::watch;

/// Holds publish work before it enters the SDK queue.
///
/// The probe controls scheduling only. Production still owns admission,
/// submission, cancellation, telemetry, and settlement.
#[doc(hidden)]
pub struct NatsPublishProbe {
    reached: watch::Receiver<bool>,
    release: watch::Sender<bool>,
}

impl NatsPublishProbe {
    /// Attach a checkpoint before cloning `connection`.
    /// Returns `None` if another connection handle already shares its state.
    #[must_use]
    pub fn hold(connection: &mut Connection) -> Option<Self> {
        let (arrived, reached) = watch::channel(false);
        let (release, released) = watch::channel(false);
        connection.hold_publish(Arc::new(PublishCheckpoint { arrived, released }))?;
        Some(Self { reached, release })
    }

    /// Resolve when publish work reaches the checkpoint.
    /// Returns false if the connection ended before reaching it.
    pub async fn reached(&mut self) -> bool {
        self.reached.wait_for(|arrived| *arrived).await.is_ok()
    }

    /// Let held work proceed through the real SDK submission path.
    pub fn release(self) {
        self.release.send_replace(true);
    }
}

/// The production half of the scheduling checkpoint.
pub(crate) struct PublishCheckpoint {
    arrived: watch::Sender<bool>,
    released: watch::Receiver<bool>,
}

impl PublishCheckpoint {
    pub(crate) async fn wait(&self) {
        self.arrived.send_replace(true);
        wait_released(&self.released).await;
    }
}

/// Wait until a probe releases held work. A dropped probe releases it too.
pub(super) async fn wait_released(released: &watch::Receiver<bool>) {
    let mut released = released.clone();
    drop(released.wait_for(|released| *released).await);
}
