//! The finite deadline each registered resource's lifecycle callbacks run under.

use super::ResourcePhase;
use crate::RuntimeError;
use crate::http::policy_value::finite_duration;
use std::time::Duration;

/// The deadline every resource phase starts at when no caller names one.
///
/// Thirty seconds, the figure the server policy's shutdown deadline and every
/// proxy deadline already default to. A resource callback is synchronous
/// application code, so the default is the family's and not a shorter number
/// invented here.
pub const DEFAULT_RESOURCE_PHASE_DEADLINE: Duration = Duration::from_secs(30);

/// The time one registered resource may spend inside each lifecycle callback.
///
/// Three independent finite deadlines, one per [`ResourcePhase`]. None of them
/// is optional: an unbounded resource callback is a runtime that cannot finish
/// starting or finish stopping, so "unbounded" has no spelling here — unlike
/// the request and transfer budgets, whose peers can legitimately stream
/// forever.
///
/// The runtime's aggregate shutdown deadline stays an outer ceiling above all
/// three. A callback receives the smaller of its phase deadline and the time
/// the aggregate has left, which is what [`phase_deadline`](Self::phase_deadline)
/// answers.
///
/// The value is small, immutable, and `Copy`: it is read once per callback and
/// shares no allocation.
///
/// ```rust
/// use camber::{ResourceBudget, ResourcePhase};
/// use std::time::Duration;
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let budget = ResourceBudget::bounded(
///     Duration::from_secs(5),
///     Duration::from_secs(2),
///     Duration::from_secs(10),
/// )?;
/// assert_eq!(budget.phase(ResourcePhase::StartupHealth), Duration::from_secs(5));
///
/// // An aggregate shutdown with three seconds left narrows the phase deadline.
/// assert_eq!(
///     budget.phase_deadline(ResourcePhase::Shutdown, Some(Duration::from_secs(3))),
///     Duration::from_secs(3),
/// );
///
/// // A spent aggregate still leaves the forced-join grace, so an owner is
/// // asked to stop rather than reported outstanding unasked.
/// assert_eq!(
///     budget.phase_deadline(ResourcePhase::Shutdown, Some(Duration::ZERO)),
///     Duration::from_millis(100),
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceBudget {
    startup_health: Duration,
    periodic_health: Duration,
    shutdown: Duration,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            startup_health: DEFAULT_RESOURCE_PHASE_DEADLINE,
            periodic_health: DEFAULT_RESOURCE_PHASE_DEADLINE,
            shutdown: DEFAULT_RESOURCE_PHASE_DEADLINE,
        }
    }
}

impl ResourceBudget {
    /// Bound all three resource phases.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] naming the phase when any
    /// duration is zero or longer than the thirty-year ceiling every Camber
    /// deadline shares. Zero would admit no callback at all, which is never
    /// what a caller configuring a resource meant.
    pub fn bounded(
        startup_health: Duration,
        periodic_health: Duration,
        shutdown: Duration,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            startup_health: finite_duration(startup_health, "resource startup_health")?,
            periodic_health: finite_duration(periodic_health, "resource periodic_health")?,
            shutdown: finite_duration(shutdown, "resource shutdown")?,
        })
    }

    /// The configured deadline for the initial readiness probe.
    #[must_use]
    pub const fn startup_health(&self) -> Duration {
        self.startup_health
    }

    /// The configured deadline for one periodic health probe.
    #[must_use]
    pub const fn periodic_health(&self) -> Duration {
        self.periodic_health
    }

    /// The configured deadline for the teardown callback.
    #[must_use]
    pub const fn shutdown(&self) -> Duration {
        self.shutdown
    }

    /// The configured deadline for `phase`.
    ///
    /// The one dispatch from a phase to its duration. A coordinator that
    /// re-derived the mapping per call site would be a second place for the
    /// three phases and the three deadlines to drift apart.
    #[must_use]
    pub const fn phase(&self, phase: ResourcePhase) -> Duration {
        match phase {
            ResourcePhase::StartupHealth => self.startup_health,
            ResourcePhase::PeriodicHealth => self.periodic_health,
            ResourcePhase::Shutdown => self.shutdown,
        }
    }

    /// The deadline one callback of `phase` actually runs under, under an outer
    /// aggregate that has `aggregate_remaining` left.
    ///
    /// The one call the shutdown coordinator bounds each teardown callback
    /// through, and the arithmetic is the aggregate's own rather than a second
    /// copy of it: the phase deadline narrows what the aggregate has left, a
    /// spent aggregate still leaves the fixed forced-join grace, and `None` —
    /// a startup or periodic probe with no aggregate above it — narrows
    /// nothing.
    #[must_use]
    pub fn phase_deadline(
        &self,
        phase: ResourcePhase,
        aggregate_remaining: Option<Duration>,
    ) -> Duration {
        super::shutdown::narrowed(self.phase(phase), aggregate_remaining)
    }
}
