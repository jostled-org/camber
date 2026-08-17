//! Shared fixture owners for the bounded-service-operation proofs.
//!
//! One owner per resource these cases need and cannot take from production.
//! Nothing here produces a service outcome, writes a production observation, or
//! performs production cleanup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

/// How long a stopped load thread has to observe its signal and exit.
const JOIN_BOUND: Duration = Duration::from_secs(3);

/// How long the fixture waits for every started thread to report entry.
const START_BOUND: Duration = Duration::from_secs(3);

/// How much arithmetic one load thread runs between stop checks.
const WORK_PER_CHECK: usize = 10_000;

/// Busy threads that give a CPU profiler stacks to sample.
///
/// A rendered profile is made of what the sampler found, and the sampler is
/// driven by the process's own CPU time: an idle process renders nothing at all.
/// Every case that measures a rendered profile therefore owns the load it is
/// rendered from rather than borrowing whatever else the suite happens to be
/// running at the time.
///
/// One owner, one stop signal, one bounded join per thread. Dropping stops and
/// joins, so an assertion that unwinds past [`Self::stop`] leaves nothing
/// running.
pub struct CpuLoad {
    running: Arc<AtomicBool>,
    /// The started threads, taken one at a time as each is joined. Fixed in
    /// length from construction: only the handles inside it are moved out.
    threads: Box<[Option<std::thread::JoinHandle<()>>]>,
    cleanup: Arc<CpuLoadCleanup>,
}

/// What one load fixture's teardown recorded, written by the teardown itself.
#[derive(Default)]
struct CpuLoadCleanup {
    exited: AtomicUsize,
    joined: AtomicUsize,
    failures: AtomicUsize,
}

/// A read-only view of one load fixture's teardown.
///
/// Held apart from the fixture so a case can prove the teardown ran after the
/// fixture itself is gone — which is the only way to state what dropping it did.
pub struct CpuLoadProbe {
    state: Arc<CpuLoadCleanup>,
}

impl CpuLoadProbe {
    /// How many load threads observed the stop signal and left their loop.
    pub fn exited(&self) -> usize {
        self.state.exited.load(Ordering::Acquire)
    }

    /// How many load threads were joined inside the bound.
    pub fn joined(&self) -> usize {
        self.state.joined.load(Ordering::Acquire)
    }

    /// How many load threads failed to be joined inside the bound.
    pub fn failures(&self) -> usize {
        self.state.failures.load(Ordering::Acquire)
    }
}

impl CpuLoad {
    /// Start `threads` busy threads and return once every one of them is running.
    ///
    /// The wait is each thread's own entry report, so a case that starts load
    /// before a measurement is not inferring readiness from a scheduler delay.
    pub fn start(threads: usize) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let cleanup = Arc::new(CpuLoadCleanup::default());
        let (started, entries) = std::sync::mpsc::sync_channel(threads);
        let handles: Vec<Option<std::thread::JoinHandle<()>>> = (0..threads)
            .map(|_| {
                let running = Arc::clone(&running);
                let cleanup = Arc::clone(&cleanup);
                let started = started.clone();
                Some(std::thread::spawn(move || {
                    started.send(()).expect("the load fixture awaits entry");
                    burn(&running);
                    cleanup.exited.fetch_add(1, Ordering::Release);
                }))
            })
            .collect();
        drop(started);
        let load = Self {
            running,
            threads: handles.into_boxed_slice(),
            cleanup,
        };
        for _ in 0..threads {
            entries
                .recv_timeout(START_BOUND)
                .expect("every load thread reported entry");
        }
        load
    }

    /// A read-only view of what this fixture's teardown records.
    pub fn probe(&self) -> CpuLoadProbe {
        CpuLoadProbe {
            state: Arc::clone(&self.cleanup),
        }
    }

    /// Stop the load and require every thread to have been joined.
    ///
    /// Dropping does the same work. A case whose measurement depends on the load
    /// being gone needs the failure reported rather than swallowed by a
    /// destructor.
    pub fn stop(mut self) {
        assert_eq!(self.shutdown(), 0, "every load thread must join");
    }

    /// Signal every thread and join it, reporting how many could not be joined.
    fn shutdown(&mut self) -> usize {
        self.running.store(false, Ordering::Release);
        let cleanup = Arc::clone(&self.cleanup);
        self.threads
            .iter_mut()
            .filter(|thread| thread.is_some())
            .map(|thread| join_recorded(thread, &cleanup))
            .sum()
    }
}

/// Join one load thread inside the bound, recording which way it went.
fn join_recorded(
    thread: &mut Option<std::thread::JoinHandle<()>>,
    cleanup: &CpuLoadCleanup,
) -> usize {
    match super::stream::join_thread_bounded(thread, JOIN_BOUND) {
        Ok(()) => {
            cleanup.joined.fetch_add(1, Ordering::Release);
            0
        }
        Err(_) => {
            cleanup.failures.fetch_add(1, Ordering::Release);
            1
        }
    }
}

impl Drop for CpuLoad {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Consume CPU until the stop signal is observed.
///
/// The stop flag is read once per batch rather than once per operation, because
/// the point of the loop is to keep a core busy: a check per multiply would spend
/// the fixture's time on the check.
fn burn(running: &AtomicBool) {
    let mut value = 0_u64;
    while running.load(Ordering::Relaxed) {
        for _ in 0..WORK_PER_CHECK {
            value = value
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
        }
    }
    std::hint::black_box(value);
}
