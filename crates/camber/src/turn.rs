//! Fair admission to a service resource with queue accounting.

mod refused;
mod wait;

pub use refused::TurnRefused;

use crate::RuntimeError;
use std::fmt;
use std::future::{Future, pending, poll_fn};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use wait::TurnWait;

struct TurnState {
    permits: Arc<Semaphore>,
    queued: AtomicUsize,
}

/// Fair access to a resource shared by service requests.
///
/// Clones share capacity, queue accounting, and closure. FIFO order follows
/// acquisition registration when futures are polled, not HTTP arrival time.
/// No background task is created and untimed waits need no runtime context.
#[derive(Clone)]
pub struct Turn {
    state: Arc<TurnState>,
}

impl fmt::Debug for Turn {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Turn")
            .field("queued", &self.queued())
            .field("closed", &self.is_closed())
            .finish()
    }
}

/// An owned turn. Dropping it releases capacity for the next waiter.
///
/// Move this guard into work that must keep exclusive access after its result
/// waiter is cancelled. It does not borrow the handle that admitted it.
#[must_use = "hold the guard for the lifetime of the resource operation"]
pub struct TurnGuard {
    permit: OwnedSemaphorePermit,
}

impl fmt::Debug for TurnGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnGuard")
            .field("permits", &self.permit.num_permits())
            .finish()
    }
}

impl Default for Turn {
    fn default() -> Self {
        Self::new()
    }
}

impl Turn {
    /// Allow one resource holder at a time.
    pub fn new() -> Self {
        Self::with_valid_capacity(1)
    }

    /// Allow `capacity` holders. Zero and values above the semaphore limit fail.
    pub fn with_capacity(capacity: usize) -> Result<Self, RuntimeError> {
        match capacity {
            1..=Semaphore::MAX_PERMITS => Ok(Self::with_valid_capacity(capacity)),
            _ => Err(RuntimeError::InvalidArgument(
                format!(
                    "turn capacity must be between 1 and {}",
                    Semaphore::MAX_PERMITS
                )
                .into_boxed_str(),
            )),
        }
    }

    fn with_valid_capacity(capacity: usize) -> Self {
        Self {
            state: Arc::new(TurnState {
                permits: Arc::new(Semaphore::new(capacity)),
                queued: AtomicUsize::new(0),
            }),
        }
    }

    /// Registered acquisitions that have not delivered a guard or refusal.
    ///
    /// A reserved permit still counts until its future is polled to completion.
    /// Unpolled acquisitions and delivered guards do not count.
    pub fn queued(&self) -> usize {
        self.state.queued.load(Ordering::Acquire)
    }

    /// Stop new admission and wake queued callers with [`TurnRefused::Closed`].
    /// Already delivered guards remain valid until their owners drop them.
    pub fn close(&self) {
        self.state.permits.close();
    }

    /// Whether any clone closed admission.
    pub fn is_closed(&self) -> bool {
        self.state.permits.is_closed()
    }

    /// Wait in registration order. Dropping the future removes its queue entry.
    pub async fn acquire(&self) -> Result<TurnGuard, RuntimeError> {
        let acquired = Arc::clone(&self.state.permits).acquire_owned();
        tokio::pin!(acquired);
        let mut waiting = TurnWait::new(&self.state.queued);
        poll_fn(|context| match acquired.as_mut().poll(context) {
            Poll::Ready(result) => {
                waiting.finish();
                Poll::Ready(
                    result
                        .map(|permit| TurnGuard { permit })
                        .map_err(|_| TurnRefused::Closed.into()),
                )
            }
            Poll::Pending => {
                waiting.register();
                Poll::Pending
            }
        })
        .await
    }

    /// Wait until granted, cancelled, or the absolute deadline expires.
    ///
    /// Cancellation wins over an expired deadline, which wins over a grant.
    /// Any cancellation-future output is accepted, including a disconnect cause.
    /// These ready outcomes need no timer. A wait that stays pending needs
    /// Tokio's timer driver, as `tokio::time::sleep` does. Without a Tokio
    /// context it returns [`RuntimeError::NoRuntime`].
    ///
    /// # Panics
    ///
    /// A pending timed wait panics when the current Tokio runtime has its timer
    /// driver disabled. Tokio reports that state only by panicking. Camber
    /// runtimes always enable the timer driver.
    pub async fn acquire_until<C>(
        &self,
        deadline: Option<Instant>,
        cancel: C,
    ) -> Result<TurnGuard, RuntimeError>
    where
        C: Future,
    {
        let acquired = self.acquire();
        let timer = wait_deadline(deadline);
        tokio::pin!(cancel, acquired, timer);
        poll_fn(
            |context| match ready_refusal(cancel.as_mut(), deadline, context) {
                Some(refusal) => Poll::Ready(Err(refusal.into())),
                None => poll_grant(acquired.as_mut(), timer.as_mut(), context),
            },
        )
        .await
    }
}

/// The refusal that is ready before any grant: cancellation, then expiry.
///
/// The deadline is compared with Tokio's clock directly, so an expired wait is
/// refused even when the timer would round its wake up to the next tick.
fn ready_refusal<C: Future>(
    cancel: Pin<&mut C>,
    deadline: Option<Instant>,
    context: &mut Context<'_>,
) -> Option<TurnRefused> {
    match cancel.poll(context).is_ready() {
        true => Some(TurnRefused::Cancelled),
        false => deadline
            .filter(|at| tokio::time::Instant::now() >= tokio::time::Instant::from_std(*at))
            .map(|_| TurnRefused::Deadline),
    }
}

/// Poll the acquisition, and arm the deadline timer only while it waits.
fn poll_grant<A, T>(
    acquired: Pin<&mut A>,
    timer: Pin<&mut T>,
    context: &mut Context<'_>,
) -> Poll<Result<TurnGuard, RuntimeError>>
where
    A: Future<Output = Result<TurnGuard, RuntimeError>>,
    T: Future<Output = Result<(), RuntimeError>>,
{
    match acquired.poll(context) {
        Poll::Ready(result) => Poll::Ready(result),
        Poll::Pending => timer
            .poll(context)
            .map(|result| result.and(Err(TurnRefused::Deadline.into()))),
    }
}

/// Sleep until `deadline`, or wait forever without one.
///
/// Built on its first poll, after every ready outcome, so only a wait that
/// must sleep needs a timer. A missing Tokio context is a typed error. A
/// runtime without its timer driver panics inside Tokio's sleep constructor.
async fn wait_deadline(deadline: Option<Instant>) -> Result<(), RuntimeError> {
    let at = match deadline {
        Some(at) => at,
        None => return pending().await,
    };
    tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
    Ok(())
}
