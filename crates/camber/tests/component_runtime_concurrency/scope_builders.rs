use camber::runtime::{self, RuntimeBuilder};
use camber::runtime_test_support::{RuntimeController, runtime_schedule};
use std::sync::Arc;
use std::time::Duration;

/// Worker threads every runtime-scope case runs on.
///
/// Enough that a blocked observer cannot starve the child it is observing.
///
/// Stated once for both roots that host these cases. A case that restated the
/// literal would keep the old count when this one moves, and the starvation
/// that follows does not announce itself — it hangs the binary instead of
/// failing an assertion. The support layer names the same count for the cases
/// that go through `observe_armed_sequence`; it is private there, so the cases
/// that build their own runtime name it here.
const WORKER_THREADS: usize = 4;

/// The builder every runtime-scope case starts from.
///
/// `shutdown_timeout` is the one thing these cases genuinely vary: a drain
/// under proof needs a boundary it can reach, and a drain that is only a hang
/// guard needs one it never should.
pub fn scope_runtime(shutdown_timeout: Duration) -> RuntimeBuilder {
    runtime::builder()
        .worker_threads(WORKER_THREADS)
        .shutdown_timeout(shutdown_timeout)
}

/// [`scope_runtime`] with a scheduling controller already attached, for a case
/// whose only use of the seam is to read the scope through it.
///
/// The controller is shared rather than borrowed because a resource hook or a
/// spawned observer reads it alongside the closure, and one `Arc` covers every
/// such reader. The builder is returned unfinished so the case keeps chaining
/// whatever else it configures.
pub fn probed_runtime(shutdown_timeout: Duration) -> (Arc<RuntimeController>, RuntimeBuilder) {
    let controller = Arc::new(runtime_schedule());
    let builder = scope_runtime(shutdown_timeout).with_test_schedule(&controller);
    (controller, builder)
}
