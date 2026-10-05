//! Runtime integration admission, entered from external tests.

#[cfg(feature = "dns01")]
use crate::integration_lifecycle::CleanupRegister;
use crate::integration_lifecycle::{IntegrationAccess, IntegrationEntryObserver, OperationWaiter};
use crate::{IntegrationError, IntegrationKind, IntegrationOperation, RuntimeError};
use std::future::Future;

/// The operation label a controlled operation reserves its account under.
///
/// The probe drives admission and settlement, not an adapter, so its work has
/// no operation of its own; it is charged as the publish an adapter submits.
const PROBE_OPERATION: IntegrationOperation = IntegrationOperation::Publish;

/// The operation label a retained probe operation reserves its account under:
/// the DNS-01 order is the work that keeps a cleanup register.
#[cfg(feature = "dns01")]
const RETAINED_PROBE_OPERATION: IntegrationOperation = IntegrationOperation::Provision;

/// The controlled integration's operation limit.
///
/// The probe proves admission and settlement, not an adapter's own limit, so
/// only the report budget bounds its operations.
const PROBE_OPERATION_LIMIT: usize = usize::MAX;

/// Calls the real runtime admission path for one fixed integration kind.
///
/// The probe chooses no outcome. The current runtime's registry admits or
/// refuses, and the entry it returns commits closing and settles itself. It
/// adds no kind, scheduler, or alternate model.
#[doc(hidden)]
pub struct IntegrationLifecycleProbe;

impl IntegrationLifecycleProbe {
    /// Admit one controlled instance of `kind` to the current runtime.
    ///
    /// # Errors
    ///
    /// `NoRuntime` outside every runtime, then the registry's own refusal:
    /// `ScopeClosed`, `Busy`, or `LimitExceeded`.
    pub fn admit(kind: IntegrationKind) -> Result<IntegrationProbeHandle, RuntimeError> {
        crate::runtime::runtime_context()?
            .admit_integration(kind, IntegrationOperation::Connect, PROBE_OPERATION_LIMIT)
            .map(|access| IntegrationProbeHandle { access })
    }

    /// Advance the current runtime's registry to an unused identity.
    ///
    /// # Errors
    ///
    /// `NoRuntime` outside every runtime. `Config` for rewind or exhausted identities.
    pub fn seed_next_identity(id: u64) -> Result<(), RuntimeError> {
        crate::runtime::runtime_context()?
            .integrations()
            .seed_next_identity(id)
    }
}

/// Revocable access to one controlled integration.
///
/// Every call is the production access handle's own. Dropping the last clone
/// requests close.
#[doc(hidden)]
#[derive(Clone)]
pub struct IntegrationProbeHandle {
    access: IntegrationAccess,
}

impl IntegrationProbeHandle {
    /// The runtime-local identity admission assigned.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.access.id()
    }

    /// A read-only view of the entry's committed state.
    #[must_use]
    pub fn observer(&self) -> IntegrationEntryObserver {
        self.access.observer()
    }

    /// Record that the controlled integration established readiness.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed.
    pub fn ready(&self) -> Result<(), RuntimeError> {
        self.access.ready()
    }

    /// Report settlement as this instance's `Close` terminal, as a DNS-01
    /// owner does from admission.
    #[cfg(feature = "dns01")]
    pub fn reports_close(&self) {
        self.access.reports_close();
    }

    /// Admit `work` as one operation of this integration.
    ///
    /// # Errors
    ///
    /// The entry's refusal: `Closed`, `Busy`, or the root scope's
    /// `ScopeClosed`.
    pub fn run<W>(&self, work: W) -> Result<OperationWaiter<()>, RuntimeError>
    where
        W: Future<Output = Result<(), IntegrationError>> + Send + 'static,
    {
        self.access.run(PROBE_OPERATION, work)
    }

    /// Admit the work `build` makes as one retained operation of this
    /// integration, which keeps a cleanup register as a DNS-01 order does.
    ///
    /// `build` receives the operation's register. The work runs and settles
    /// through the production retained path.
    ///
    /// # Errors
    ///
    /// The entry's refusal: `Closed`, `Busy`, or the root scope's
    /// `ScopeClosed`.
    #[cfg(feature = "dns01")]
    pub fn run_retained<B, W>(&self, build: B) -> Result<OperationWaiter<()>, RuntimeError>
    where
        B: FnOnce(ProbeCleanup) -> W,
        W: Future<Output = Result<(), IntegrationError>> + Send + 'static,
    {
        self.access
            .admit(RETAINED_PROBE_OPERATION)?
            .submit_retained(|_, cleanup| build(ProbeCleanup { cleanup }))
    }

    /// Request close and resolve with the entry's one fixed close result.
    pub fn close(&self) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'static + use<> {
        self.access.close()
    }
}

/// The cleanup register of one retained probe operation.
///
/// Each call is the register's own transition, as an order makes it.
#[cfg(feature = "dns01")]
#[doc(hidden)]
pub struct ProbeCleanup {
    cleanup: CleanupRegister,
}

#[cfg(feature = "dns01")]
impl ProbeCleanup {
    /// Record `domain`'s record as created and acknowledged under `id`, as an
    /// order does once the provider answers its create.
    ///
    /// `false` when the register is full and names nothing more.
    #[must_use]
    pub fn acknowledged(&self, domain: &str, id: &str) -> bool {
        self.cleanup
            .intend(domain)
            .map(|slot| self.cleanup.acknowledge(slot, id))
            .is_some()
    }
}
