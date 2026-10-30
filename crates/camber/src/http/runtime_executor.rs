use crate::runtime_state::{RuntimeInner, carry_runtime};
use hyper::rt::Executor;
use std::future::Future;
use std::sync::Arc;

/// Carry the server's captured authority through Hyper's protocol tasks.
///
/// Hyper owns these tasks. User tasks admitted while they run still belong to
/// the captured Camber root scope, including tasks started by response bodies.
#[derive(Clone)]
pub(super) struct RuntimeExecutor {
    runtime: Option<Arc<RuntimeInner>>,
}

impl RuntimeExecutor {
    pub(super) fn new(runtime: Option<Arc<RuntimeInner>>) -> Self {
        Self { runtime }
    }
}

impl<F> Executor<F> for RuntimeExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        hyper_util::rt::TokioExecutor::new().execute(carry_runtime(self.runtime.clone(), future));
    }
}
