use crate::runtime_state::{LatchSignal, LifecycleSignals, RuntimeInner};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// What a watcher applies when SIGINT/SIGTERM arrives.
///
/// A runtime-owned watcher applies the whole shutdown request, so an OS signal
/// disposes of admission exactly as `runtime::request_shutdown` does — the
/// "shutdown implies scope closing" definition stays in `RuntimeInner`, and no
/// spawn is admitted after a SIGTERM that a `request_shutdown` would have
/// refused. The public wrapper owns no runtime, so it fires the caller's latch
/// alone.
pub(crate) enum ShutdownRequest {
    Runtime(Arc<RuntimeInner>),
    Latch(LatchSignal),
}

impl ShutdownRequest {
    fn apply(&self) {
        match self {
            Self::Runtime(runtime) => runtime.request_shutdown(),
            Self::Latch(latch) => latch.fire(),
        }
    }
}

/// The OS signal sources a watcher observes.
///
/// Registered synchronously, before the watcher is spawned or admitted, so a
/// signal raised immediately afterwards is not missed by a handler that had
/// not been installed yet.
pub(crate) struct SignalSources {
    sigint: Option<Signal>,
    sigterm: Option<Signal>,
}

impl SignalSources {
    pub(crate) fn register() -> Self {
        Self {
            sigint: Self::register_one(SignalKind::interrupt(), "SIGINT"),
            sigterm: Self::register_one(SignalKind::terminate(), "SIGTERM"),
        }
    }

    /// A *registration error* is survivable — the watcher simply never wakes on
    /// that signal — but it is never routine: a process that failed to register
    /// SIGTERM has silently lost graceful shutdown, so the error is reported
    /// rather than collapsed into `None` unremarked.
    ///
    /// This covers only the errors `signal` returns. It does NOT cover a
    /// missing reactor: with no Tokio runtime on the current thread `signal`
    /// unwinds before the match, so absence of a runtime is a panic in the
    /// caller, not a `None` here. Every caller reaches this from a `# Panics`
    /// documented entry point.
    fn register_one(kind: SignalKind, name: &'static str) -> Option<Signal> {
        match signal(kind) {
            Ok(source) => Some(source),
            Err(error) => {
                tracing::warn!(signal = name, %error, "OS signal registration failed");
                None
            }
        }
    }

    /// Wait for the first SIGINT or SIGTERM, and report which one arrived.
    ///
    /// The winning arm is the only place that knows what ended the wait, so it
    /// hands the name back rather than discarding it — nothing downstream can
    /// recover it, and a drain that begins with no record of what signalled it
    /// is the log an operator is left reading.
    ///
    /// With both sources unregistered the wait parks forever and there is no
    /// name to return, which is why that arm's return type is the never type
    /// rather than a placeholder name for a signal that did not arrive.
    async fn wait(mut self) -> &'static str {
        match (&mut self.sigint, &mut self.sigterm) {
            (Some(sigint), Some(sigterm)) => tokio::select! {
                _ = sigint.recv() => "SIGINT",
                _ = sigterm.recv() => "SIGTERM",
            },
            (Some(sigint), None) => {
                sigint.recv().await;
                "SIGINT"
            }
            (None, Some(sigterm)) => {
                sigterm.recv().await;
                "SIGTERM"
            }
            (None, None) => std::future::pending().await,
        }
    }
}

/// Spawn an async task that watches for OS signals.
///
/// The returned handle is caller-owned: this task is not admitted to the
/// runtime's root scope, because the scope cannot single-own a handle it also
/// hands back. The runtime's own watcher is scope-owned instead, and it
/// applies the whole shutdown request rather than the caller's latch alone.
///
/// The task has TWO completion paths, and they are not interchangeable:
///
/// - an OS SIGINT/SIGTERM arrives — `shutdown` is set to true and waiters are
///   notified;
/// - inside a Camber runtime, the root scope closes or shutdown latches — the
///   watch simply ends and `shutdown` is left untouched, so the watcher does
///   not outlive the runtime that hosts it.
///
/// A caller must therefore READ `shutdown` rather than infer it from the
/// handle resolving: completion alone does not mean a signal arrived. Spawned
/// outside a Camber runtime the captured signals are inert, so only an OS
/// signal ever ends the watch.
///
/// The `notify` half is a wake, not a record. Firing wakes the waiters
/// registered at that moment and stores no permit, so a `notified()` registered
/// after the signal lands never resolves — the flag is the durable half and the
/// notification is lossy for late waiters. Register the wait BEFORE checking
/// `shutdown`, then read the flag: a fire cannot slip between the two, whereas
/// checking first and then waiting can miss it forever.
///
/// # Panics
///
/// Panics if called outside a Tokio runtime context. Both halves of this
/// function need one and neither degrades: the OS handlers are installed
/// synchronously here, and `tokio::signal::unix::signal` panics with no reactor
/// on the current thread; `tokio::spawn` then panics with no runtime to admit
/// the watcher to. Call it from inside `runtime::run`, or from any other Tokio
/// runtime context.
pub fn spawn_signal_watcher(
    shutdown: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(signal_watcher_loop(
        SignalSources::register(),
        // This boundary is the only one that holds the latch as two halves, so
        // it is the only one that reassembles them.
        ShutdownRequest::Latch(LatchSignal::from_parts(shutdown, notify)),
        LifecycleSignals::current(),
    ))
}

/// Watch for SIGINT/SIGTERM until one arrives or a lifecycle signal ends the
/// watch.
///
/// The OS signal is an external edge that may never fire, so the watch needs
/// its own exit arm: without one a scope-owned watcher would never let the
/// drain finish. `guard` is that arm, and the one definition of racing work
/// against the lifecycle signals — a lifecycle exit leaves the shutdown
/// request unapplied, because no OS signal arrived to apply.
///
/// The signal is recorded here because this is the only place that knows it.
/// `RuntimeInner::request_shutdown` applies a request without caring where it
/// came from, so without this line a SIGTERM'd process logs a drain that starts
/// for no stated reason.
///
/// The record covers the repeat signal too. This watcher is one-shot, and
/// tokio's handler stays installed for the process lifetime once registered, so
/// a second Ctrl-C is swallowed rather than ending the process — the operator
/// waiting on a slow drain needs to be told that up front.
pub(crate) async fn signal_watcher_loop(
    sources: SignalSources,
    shutdown: ShutdownRequest,
    signals: LifecycleSignals,
) {
    match signals.guard(sources.wait()).await {
        ControlFlow::Continue(signal) => {
            tracing::info!(
                signal,
                "shutdown requested; repeat signals are ignored, SIGKILL to force"
            );
            shutdown.apply();
        }
        ControlFlow::Break(()) => {}
    }
}
