use crate::RuntimeError;
use std::sync::{Arc, Condvar, Mutex};

/// Runtime scheduling points exposed only for deterministic integration tests.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCheckpoint {
    /// The shutdown notification is registered, before sticky state is read.
    ShutdownWaitRegistered,
    /// A tracked-task count was observed before the runtime waits for change.
    TaskWaitPredicateObserved(usize),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CheckpointPhase {
    Armed,
    Paused,
    Released,
}

struct CheckpointState {
    closed: bool,
    checkpoint: Option<(RuntimeCheckpoint, CheckpointPhase)>,
}

pub(crate) struct RuntimeSchedule {
    state: Mutex<CheckpointState>,
    changed: Condvar,
}

impl RuntimeSchedule {
    fn new() -> Self {
        Self {
            state: Mutex::new(CheckpointState {
                closed: false,
                checkpoint: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn invalid(message: &'static str) -> RuntimeError {
        RuntimeError::InvalidArgument(message.into())
    }

    fn arm(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match (state.closed, state.checkpoint) {
            (true, _) => Err(Self::invalid("runtime scheduling controller is closed")),
            (false, Some((_, CheckpointPhase::Armed | CheckpointPhase::Paused))) => Err(
                Self::invalid("runtime scheduling checkpoint is already armed"),
            ),
            (false, None | Some((_, CheckpointPhase::Released))) => {
                state.checkpoint = Some((checkpoint, CheckpointPhase::Armed));
                Ok(())
            }
        }
    }

    pub(crate) fn is_armed(&self, checkpoint: RuntimeCheckpoint) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        matches!(
            (state.closed, state.checkpoint),
            (false, Some((armed, CheckpointPhase::Armed))) if armed == checkpoint
        )
    }

    pub(crate) fn pause(&self, checkpoint: RuntimeCheckpoint) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match (state.closed, state.checkpoint) {
            (false, Some((armed, CheckpointPhase::Armed))) if armed == checkpoint => {
                state.checkpoint = Some((checkpoint, CheckpointPhase::Paused));
                self.changed.notify_all();
            }
            _ => return,
        }

        while matches!(
            state.checkpoint,
            Some((paused, CheckpointPhase::Paused)) if paused == checkpoint
        ) && !state.closed
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn wait_until_paused(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            match (state.closed, state.checkpoint) {
                (true, _) => {
                    return Err(Self::invalid("runtime scheduling controller is closed"));
                }
                (false, Some((paused, CheckpointPhase::Paused))) if paused == checkpoint => {
                    return Ok(());
                }
                (false, Some((released, CheckpointPhase::Released))) if released == checkpoint => {
                    return Err(Self::invalid(
                        "runtime scheduling checkpoint was already released",
                    ));
                }
                (false, Some((armed, _))) if armed != checkpoint => {
                    return Err(Self::invalid(
                        "runtime scheduling checkpoint does not match the armed checkpoint",
                    ));
                }
                (false, None) => {
                    return Err(Self::invalid("runtime scheduling checkpoint is not armed"));
                }
                (false, Some(_)) => {
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(|error| error.into_inner());
                }
            }
        }
    }

    fn release(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match (state.closed, state.checkpoint) {
            (true, _) => Err(Self::invalid("runtime scheduling controller is closed")),
            (false, Some((paused, CheckpointPhase::Paused))) if paused == checkpoint => {
                state.checkpoint = Some((checkpoint, CheckpointPhase::Released));
                self.changed.notify_all();
                Ok(())
            }
            (false, Some((armed, _))) if armed != checkpoint => Err(Self::invalid(
                "runtime scheduling checkpoint does not match the armed checkpoint",
            )),
            (false, _) => Err(Self::invalid("runtime scheduling checkpoint is not paused")),
        }
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        self.changed.notify_all();
    }
}

/// Controller for one runtime instance's deterministic scheduling seam.
#[doc(hidden)]
pub struct RuntimeController {
    schedule: Arc<RuntimeSchedule>,
}

impl RuntimeController {
    /// Arm one value-matched checkpoint.
    pub fn pause_once(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        self.schedule.arm(checkpoint)
    }

    /// Block until production code reaches the armed checkpoint.
    pub fn wait_until_paused(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        self.schedule.wait_until_paused(checkpoint)
    }

    /// Release production code from the paused checkpoint.
    pub fn release(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        self.schedule.release(checkpoint)
    }

    pub(crate) fn schedule(&self) -> Arc<RuntimeSchedule> {
        Arc::clone(&self.schedule)
    }
}

impl Drop for RuntimeController {
    fn drop(&mut self) {
        self.schedule.close();
    }
}

/// Create an unregistered controller for one runtime builder.
#[doc(hidden)]
pub fn runtime_schedule() -> RuntimeController {
    RuntimeController {
        schedule: Arc::new(RuntimeSchedule::new()),
    }
}
