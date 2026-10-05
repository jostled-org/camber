//! The owner a runtime admits for DNS-01 provisioning.
//!
//! One owner holds one integration entry, the validated plan its orders run
//! under, and the provider it took from the caller. Direct provisioning,
//! runtime startup, and both renewal paths enter orders through here, so each
//! prepares its provider under the same admitted boundary. At most one order
//! runs per owner: each borrows the provider for its whole length. A renewing
//! owner also holds its cache's claim, so no second owner of the same runtime
//! renews that cache beside it.
//!
//! An owner's settlement is its one `Close` terminal, and each cache check is
//! one `CacheRead`. A refusal before admission is the admitting operation's
//! terminal, with no instance and no duration.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use rustls::sign::CertifiedKey;

use super::cache::{read_reported, renewal_needed};
use super::certificate::{Generation, unix_now};
use super::claim::RenewalClaim;
use super::order::{OrderPlan, OrderStop, StopRequest, run_order};
use super::provider::DnsProvider;
use crate::integration_lifecycle::{
    IntegrationAccess, NestedTerminals, OperationWaiter, TerminalOperation,
};
use crate::lifecycle::{AggregateShutdown, ShutdownOwner};
use crate::runtime_state::{LifecycleSignals, RuntimeInner};
use crate::runtime_test_support::RuntimeSchedule;
use crate::task::AsyncJoinHandle;
use crate::tls::CertStore;
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, RuntimeError,
};

/// How often renewal reads the cached leaf's own `notAfter`.
const RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// One order at a time: the owner's operation limit.
const ORDERS_AT_ONCE: usize = 1;

/// An admitted DNS-01 owner.
pub(super) struct Dns01Owner<P> {
    plan: OrderPlan,
    provider: P,
    access: IntegrationAccess,
    /// The terminals of this owner's nested operations.
    terminals: NestedTerminals,
    /// The aggregate shutdown of the runtime that admitted this owner, which
    /// cuts each order's cleanup short once a stop fixes its expiry.
    shutdown: Arc<AggregateShutdown>,
    /// The test schedule whose clock renewal intervals elapse on; real time
    /// when none is attached.
    clock: Option<Arc<RuntimeSchedule>>,
    /// The cache claim of a renewing owner, held only to be given back when
    /// the owner drops; a direct owner holds none.
    _claim: Option<RenewalClaim>,
}

impl<P: DnsProvider + 'static> Dns01Owner<P> {
    /// Admit a direct owner of `plan` and `provider` to `runtime` through its
    /// `admission` operation, before any provider or cache effect.
    ///
    /// # Errors
    ///
    /// The registry's refusal: `ScopeClosed`, `Busy`, or `LimitExceeded`.
    pub(super) fn admit(
        runtime: &Arc<RuntimeInner>,
        plan: OrderPlan,
        provider: P,
        admission: IntegrationOperation,
    ) -> Result<Self, RuntimeError> {
        Self::admit_claimed(runtime, plan, provider, admission, None)
    }

    /// Admit an owner that renews `plan`'s cache, claiming the cache first.
    ///
    /// # Errors
    ///
    /// `InvalidConfig` when the cache path cannot be made absolute, `Busy`
    /// while another owner of `runtime` renews the same cache, then the
    /// registry's refusal. Each refusal leaves nothing claimed.
    pub(super) fn admit_renewing(
        runtime: &Arc<RuntimeInner>,
        plan: OrderPlan,
        provider: P,
        admission: IntegrationOperation,
    ) -> Result<Self, RuntimeError> {
        let claim = runtime.dns01_renewals().claim(&plan.cache_dir, admission)?;
        Self::admit_claimed(runtime, plan, provider, admission, Some(claim))
    }

    fn admit_claimed(
        runtime: &Arc<RuntimeInner>,
        plan: OrderPlan,
        provider: P,
        admission: IntegrationOperation,
        claim: Option<RenewalClaim>,
    ) -> Result<Self, RuntimeError> {
        let access =
            runtime.admit_integration(IntegrationKind::Dns01, admission, ORDERS_AT_ONCE)?;
        access.reports_close();
        Ok(Self {
            plan,
            provider,
            terminals: access.nested(LifecycleSignals::from_runtime(runtime)),
            access,
            shutdown: runtime.shutdown_deadline(),
            clock: runtime.renewal_schedule(),
            _claim: claim,
        })
    }

    /// The stop of one order of this owner.
    fn stop(&self, request: StopRequest) -> OrderStop {
        OrderStop::new(
            request,
            Arc::clone(&self.shutdown),
            ShutdownOwner::integration(IntegrationKind::Dns01, self.access.id()),
        )
    }

    /// Submit one direct order, which owns this owner until it settles.
    ///
    /// The waiter's drop is a stop request, not a drop of the order: the order
    /// keeps the provider and the plan until it has settled its own cleanup,
    /// and a failed cleanup stays charged after the caller reads it.
    ///
    /// # Errors
    ///
    /// `Busy` when the report budget is full, and the root scope's own
    /// refusal once its admission has closed. A refusal sends nothing.
    pub(super) fn provision(
        self,
        signals: LifecycleSignals,
    ) -> Result<OperationWaiter<CertifiedKey>, RuntimeError> {
        let admitted = self.access.admit(IntegrationOperation::Provision)?;
        admitted.submit_retained(move |waiter_gone, records| async move {
            let mut owner = self;
            let stop = owner.stop(StopRequest::new(signals, Some(waiter_gone)));
            run_order(
                &owner.plan,
                &mut owner.provider,
                &stop,
                &records,
                &owner.terminals,
            )
            .await
        })
    }

    /// The generation cached for this owner's plan, read as one reported
    /// `CacheRead`.
    ///
    /// # Errors
    ///
    /// The cache reader's `CacheRead` or `CacheWrite` failure.
    fn cached(&self) -> Result<Option<Generation>, IntegrationError> {
        read_reported(&self.plan.cache_dir, &self.plan.domains, &self.terminals)
    }

    /// Run one order as `operation` of this owner's entry, on the calling
    /// task.
    ///
    /// The order's report account is reserved before preparation, so a full
    /// budget answers `Busy` with no provider effect and no retained history.
    async fn order(
        &mut self,
        operation: IntegrationOperation,
        request: &StopRequest,
    ) -> Result<CertifiedKey, IntegrationError> {
        let admitted = self.access.admit_typed(operation)?;
        let stop = self.stop(request.clone());
        let Self {
            plan,
            provider,
            terminals,
            ..
        } = self;
        admitted
            .run_here(|records| async move {
                run_order(plan, provider, &stop, &records, terminals).await
            })
            .await
    }

    /// The certificate to serve first: a valid cached one that is not due for
    /// renewal, or a fresh order's. The owner is ready once it holds one.
    ///
    /// # Errors
    ///
    /// The first order's failure, or `Closed` when the runtime stopped the
    /// owner before it was ready.
    pub(super) async fn initial(
        &mut self,
        request: &StopRequest,
    ) -> Result<CertifiedKey, RuntimeError> {
        let key = match self.cached() {
            Ok(Some(generation)) if !generation.renewal_due(unix_now()) => generation.into_key(),
            cached => {
                log_cache_miss(&cached);
                self.order(IntegrationOperation::Provision, request)
                    .await
                    .map_err(crate::integration_lifecycle::integration)?
            }
        };
        self.access.ready()?;
        Ok(key)
    }

    /// Renew into `store` until `request` asks the loop to stop.
    ///
    /// Each interval reads the cached leaf; only a leaf due for renewal starts
    /// an order, and the next interval begins only once that order and its
    /// cleanup settled. A renewal the report budget refuses performs no
    /// provider work and waits for the next interval. A failed renewal keeps
    /// the served certificate and retries at the next interval.
    pub(super) async fn renew(mut self, store: CertStore, request: StopRequest) {
        while let ControlFlow::Continue(()) =
            request.tick(renewal_interval(self.clock.as_deref())).await
        {
            if !renewal_needed(self.cached()) {
                continue;
            }
            tracing::info!("dns01 acme: cert renewal triggered");
            match self.order(IntegrationOperation::Renew, &request).await {
                Ok(key) => {
                    store.swap(key);
                    tracing::info!("dns01 acme: cert renewed and swapped");
                }
                Err(error) if stopped(&error, &request) => return,
                // The cause is a structured field, not part of the message: one
                // condition with an interpolated cause becomes a distinct
                // message string per failure, which is what an operator filters
                // on.
                Err(error) => tracing::warn!(%error, "dns01 acme: renewal failed"),
            }
        }
    }

    /// Renew into `store` as a user-owned child of `runtime`.
    ///
    /// The handle's `cancel` asks the loop to stop: an order in progress
    /// settles its cleanup before the owner drops. The handle answers
    /// `Cancelled` after a cancel, and `Ok(())` after a runtime stop.
    pub(super) fn spawn_renewal(
        self,
        runtime: &Arc<RuntimeInner>,
        store: CertStore,
    ) -> AsyncJoinHandle<Result<(), RuntimeError>> {
        let signals = LifecycleSignals::from_runtime(runtime);
        crate::task::spawn_async_stoppable_on(runtime, move |cancel| async move {
            let request = StopRequest::new(signals, Some(cancel));
            self.renew(store, request.clone()).await;
            renewal_answer(&request)
        })
    }
}

/// Admit what `admit` builds through `admission`, reporting its refusal as
/// that operation's terminal before admission: no instance, no duration.
///
/// # Errors
///
/// The refusal `admit` returns.
pub(super) fn admitted<T>(
    admission: IntegrationOperation,
    admit: impl FnOnce() -> Result<T, RuntimeError>,
) -> Result<T, RuntimeError> {
    TerminalOperation::new(IntegrationKind::Dns01, admission, None).refusing(admit())
}

/// What a stopped public renewal answers: `Cancelled` when its caller asked
/// for the stop, `Ok(())` when its runtime did.
fn renewal_answer(request: &StopRequest) -> Result<(), RuntimeError> {
    match request.by_caller() {
        true => Err(RuntimeError::Cancelled),
        false => Ok(()),
    }
}

/// One renewal interval: real time, or the attached test schedule's clock.
async fn renewal_interval(clock: Option<&RuntimeSchedule>) {
    match clock {
        Some(schedule) => schedule.renewal_interval(RENEWAL_CHECK_INTERVAL).await,
        None => tokio::time::sleep(RENEWAL_CHECK_INTERVAL).await,
    }
}

/// Whether `error` is an order's answer to its owner's own stop.
fn stopped(error: &IntegrationError, request: &StopRequest) -> bool {
    error.failure() == IntegrationFailure::Cancelled && request.is_requested()
}

fn log_cache_miss<T>(cached: &Result<Option<T>, IntegrationError>) {
    match cached {
        Ok(None) => tracing::info!("no cached cert found, provisioning fresh certificate"),
        Ok(Some(_)) => tracing::info!("cached cert needs renewal, provisioning fresh certificate"),
        Err(error) => {
            tracing::warn!(%error, "failed to load cached cert, provisioning fresh certificate");
        }
    }
}
