use std::cell::Cell;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(1);

static INVOCATIONS: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

#[derive(Debug)]
pub struct RuntimeError;

pub fn invocation_count() -> usize {
    INVOCATIONS.load(Ordering::SeqCst)
}

pub fn is_active() -> bool {
    ACTIVE.get()
}

pub fn __test_async<F, Fut, T>(factory: F) -> Result<T, RuntimeError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    INVOCATIONS.fetch_add(1, Ordering::SeqCst);
    let context = RuntimeContext::enter();
    let output = block_on(factory());
    drop(context);
    Ok(output)
}

struct RuntimeContext;

impl RuntimeContext {
    fn enter() -> Self {
        ACTIVE.set(true);
        Self
    }
}

impl Drop for RuntimeContext {
    fn drop(&mut self) {
        ACTIVE.set(false);
    }
}

fn block_on<F>(future: F) -> F::Output
where
    F: Future,
{
    let waker = Waker::from(Arc::new(YieldWake));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);

    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

struct YieldWake;

impl Wake for YieldWake {
    fn wake(self: Arc<Self>) {
        std::thread::yield_now();
    }
}
