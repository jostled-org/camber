//! Scripted resources, for the two roots that drive the bounded resource
//! coordinator.
//!
//! One fixture, because both roots make claims about the same coordinator: a
//! component root reads the aggregate it returns, an operator-visible root
//! reads the health and events it publishes, and a second copy of "what a
//! callback did" would let one root's script drift from the other's.

use camber::{Resource, ResourceBudget, RuntimeError};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::http::{poll_until, send};

/// How long a bounded observation waits for a coordinator to reach it.
pub const OBSERVATION_BOUND: Duration = Duration::from_secs(10);

/// The interval every row's periodic coordinator runs on.
///
/// The runtime's own minimum. A row that needs two passes waits two of these,
/// so the shortest legal interval is the one every row uses.
pub const TICK: Duration = Duration::from_secs(1);

/// A budget short enough that a parked callback is abandoned inside one row.
pub fn short_resource_budget() -> ResourceBudget {
    ResourceBudget::bounded(
        Duration::from_millis(300),
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .unwrap()
}

/// Wait until `ready` holds, or fail the row rather than block forever.
pub fn wait_until(what: &str, ready: impl FnMut() -> bool) {
    assert!(
        poll_until(OBSERVATION_BOUND, ready),
        "{what} never happened within {OBSERVATION_BOUND:?}"
    );
}

/// Read the live health endpoint until `settled` accepts its answer, then hand
/// back the status and body that were read last.
///
/// Polled rather than read once: a resource only reaches a periodic state on a
/// later pass, and every row that asserts on one would otherwise race the pass
/// that produces it. The last answer is returned either way, so a row whose
/// state never arrived fails against what the endpoint actually said.
pub fn health_until(
    addr: std::net::SocketAddr,
    settled: impl Fn(u16, &str) -> bool,
) -> (u16, Box<str>) {
    let mut answer = health_once(addr);
    let deadline = std::time::Instant::now() + OBSERVATION_BOUND;
    while !settled(answer.0, &answer.1) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
        answer = health_once(addr);
    }
    answer
}

/// One live read of the health endpoint.
fn health_once(addr: std::net::SocketAddr) -> (u16, Box<str>) {
    let response = send(addr, "GET", "/health", &[], b"");
    (
        response.status,
        String::from_utf8_lossy(&response.body)
            .into_owned()
            .into_boxed_str(),
    )
}

/// A latch a test holds a callback on until it opens it.
///
/// Owned by the test, not by the runtime: every row that parks a callback opens
/// its gate before returning, so no worker is left holding a resource after the
/// assertion that needed it parked.
#[derive(Default)]
pub struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        while !*open {
            open = self.changed.wait(open).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn open(&self) {
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.changed.notify_all();
    }
}

/// Every callback begin, in the order the coordinator started them.
#[derive(Default)]
pub struct CallbackLog(Mutex<Vec<String>>);

impl CallbackLog {
    fn begin(&self, resource: &str, phase: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{resource}:{phase}"));
    }

    /// Every resource that began `phase`, in the order they began it.
    pub fn phase(&self, phase: &str) -> Box<[Box<str>]> {
        let suffix = format!(":{phase}");
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|entry| entry.strip_suffix(&suffix).map(Box::from))
            .collect()
    }

    pub fn count(&self, resource: &str, phase: &str) -> usize {
        self.phase(phase)
            .iter()
            .filter(|entry| &***entry == resource)
            .count()
    }
}

/// What one scripted callback does instead of returning, from the numbered
/// call onward. Calls are one-indexed, and the readiness pass is call one.
#[derive(Clone, Copy)]
pub enum Behavior {
    Succeed,
    FailFrom(usize),
    PanicFrom(usize),
    ParkFrom(usize),
}

/// A resource whose every callback is scripted, logged, and counted.
///
/// One fixture for every row: the phases differ only in which behavior is
/// scripted for them, and a second fixture would be a second place a "began"
/// record could drift from the callback it claims to describe.
pub struct ScriptedResource {
    name: &'static str,
    log: Arc<CallbackLog>,
    health: Behavior,
    shutdown: Behavior,
    health_calls: AtomicUsize,
    entered: Option<SyncSender<()>>,
    gate: Arc<Gate>,
    dropped: Arc<AtomicBool>,
}

impl ScriptedResource {
    pub fn new(name: &'static str, log: &Arc<CallbackLog>) -> Self {
        Self {
            name,
            log: Arc::clone(log),
            health: Behavior::Succeed,
            shutdown: Behavior::Succeed,
            health_calls: AtomicUsize::new(0),
            entered: None,
            gate: Arc::new(Gate::default()),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    #[must_use]
    pub fn health(mut self, health: Behavior) -> Self {
        self.health = health;
        self
    }

    #[must_use]
    pub fn shutdown(mut self, shutdown: Behavior) -> Self {
        self.shutdown = shutdown;
        self
    }

    /// Report each parked callback to the test, so a row waits for the park
    /// instead of sleeping until it is probably true.
    #[must_use]
    pub fn reports_parking(mut self, entered: SyncSender<()>) -> Self {
        self.entered = Some(entered);
        self
    }

    pub fn gate(&self) -> Arc<Gate> {
        Arc::clone(&self.gate)
    }

    /// The witness that this resource is still owned by somebody.
    pub fn drop_witness(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.dropped)
    }

    fn act(&self, behavior: Behavior, call: usize) -> Result<(), RuntimeError> {
        match behavior {
            Behavior::FailFrom(first) if call >= first => {
                Err(RuntimeError::InvalidArgument(SCRIPTED_REFUSAL.into()))
            }
            Behavior::PanicFrom(first) if call >= first => panic!("{SCRIPTED_PANIC}"),
            Behavior::ParkFrom(first) if call >= first => {
                self.report_parking();
                self.gate.wait();
                Ok(())
            }
            Behavior::Succeed
            | Behavior::FailFrom(_)
            | Behavior::PanicFrom(_)
            | Behavior::ParkFrom(_) => Ok(()),
        }
    }

    /// Tell the row this callback has parked, when the row asked to be told.
    ///
    /// The send is dropped rather than reported: a row that stopped listening
    /// has already left the park behind, and this is the callback's own thread.
    fn report_parking(&self) {
        if let Some(entered) = self.entered.as_ref() {
            let _reported = entered.send(());
        }
    }
}

/// The cause a scripted refusal carries. Named, because one row proves this
/// text reaches an operator event and reaches no response body.
pub const SCRIPTED_REFUSAL: &str = "scripted refusal";

/// The payload a scripted panic carries, for the same reason.
pub const SCRIPTED_PANIC: &str = "scripted resource panic";

impl Drop for ScriptedResource {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

impl Resource for ScriptedResource {
    fn name(&self) -> &str {
        self.name
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        self.log.begin(self.name, "health");
        let call = self.health_calls.fetch_add(1, Ordering::Relaxed) + 1;
        self.act(self.health, call)
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        self.log.begin(self.name, "shutdown");
        self.act(self.shutdown, 1)
    }
}
