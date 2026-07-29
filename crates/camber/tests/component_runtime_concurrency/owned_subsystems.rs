use crate::common::{
    BOUND, PERPETUAL, RUNTIME_OWNED_CHILDREN, RecordingResource, ignore_hook, prove_scope_owned,
};
#[cfg(feature = "dns01")]
use camber::RuntimeError;
use std::sync::Mutex;
use std::time::Duration;

/// The shortest interval `RuntimeBuilder::health_interval` accepts; anything
/// below it is clamped up to it.
const MIN_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// An interval schedule and a per-resource health loop are both root-scope
/// children that exit on `ScopeClosing` and are awaited, not reaped.
#[test]
fn schedules_and_resource_health_tasks_exit_on_closing_and_are_awaited() {
    let (check_tx, check_rx) = std::sync::mpsc::channel::<()>();
    let (tick_tx, tick_rx) = std::sync::mpsc::channel::<()>();
    let tick_tx = Mutex::new(tick_tx);
    // Reports every health check it is asked for, so the case can prove the
    // per-resource health loop really ticked before closing.
    let check_tx = Mutex::new(check_tx);

    let (proof, ()) = prove_scope_owned(
        BOUND,
        move |builder| {
            builder
                .resource(RecordingResource::new(
                    "ticking",
                    move || {
                        let _ = check_tx
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .send(());
                        Ok(())
                    },
                    ignore_hook,
                ))
                // The floor `health_interval` clamps to. A shorter value would
                // be silently raised back to this, so the loop's second report
                // cannot arrive any sooner than one second.
                .health_interval(MIN_HEALTH_INTERVAL)
        },
        move |_| {
            let schedule = camber::schedule::every(PERPETUAL, move || {
                // Poison-recovering, like every other lock in this suite: this
                // callback is a root-scope child, so a panic here would displace
                // `run`'s result and report an unrelated fault as a lifecycle
                // failure.
                let _ = tick_tx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .send(());
            })
            .unwrap();

            // Drive one tick of each owned loop, so both are provably alive
            // before the closure returns.
            schedule.trigger();
            tick_rx.recv_timeout(BOUND).unwrap();
            check_rx.recv_timeout(BOUND).unwrap();
            check_rx.recv_timeout(BOUND).unwrap();
        },
    );

    // Occupants: the runtime's signal watcher, the resource health loop, the
    // holder, and the interval schedule.
    proof.assert_owned(
        "the interval schedule and per-resource health loop",
        RUNTIME_OWNED_CHILDREN + 3,
    );
}

/// The OS signal watcher is a root-scope child that exits on `ScopeClosing`
/// even though its external trigger — an actual signal — never fires.
#[cfg(unix)]
#[test]
fn signal_watcher_is_scope_owned_and_exits_on_closing() {
    let (proof, ()) = prove_scope_owned(
        BOUND,
        |builder| builder,
        |_| camber::runtime_test_support::admit_signal_watcher_for_test().unwrap(),
    );

    proof.assert_owned("the signal watcher", RUNTIME_OWNED_CHILDREN + 2);
}

/// The ACME renewal loop is a root-scope child that exits on `ScopeClosing`
/// once its scripted event stream stops producing.
#[cfg(feature = "acme")]
#[test]
fn acme_renewal_is_scope_owned_and_exits_on_closing() {
    let (proof, ()) = prove_scope_owned(
        BOUND,
        |builder| builder,
        |_| {
            let events: Box<[Result<Box<str>, Box<str>>]> = Box::new([
                Ok(Box::from("cert provisioned")),
                Err(Box::from("transient directory error")),
            ]);
            camber::runtime_test_support::admit_acme_renewal_for_test(events).unwrap();
        },
    );

    proof.assert_owned("the acme renewal loop", RUNTIME_OWNED_CHILDREN + 2);
}

/// A DNS provider that answers without touching the network. The renewal
/// check interval is measured in hours, so the loop never reaches it.
#[cfg(feature = "dns01")]
struct ScriptedDnsProvider;

#[cfg(feature = "dns01")]
impl camber::dns01::DnsProvider for ScriptedDnsProvider {
    fn create_txt_record(
        &self,
        _: &str,
        _: &str,
    ) -> impl std::future::Future<Output = Result<camber::dns01::RecordId, RuntimeError>> + Send
    {
        std::future::ready(Ok(camber::dns01::RecordId::from("scripted")))
    }

    fn delete_txt_record(
        &self,
        _: &str,
    ) -> impl std::future::Future<Output = Result<(), RuntimeError>> + Send {
        std::future::ready(Ok(()))
    }
}

/// The DNS-01 renewal loop is a root-scope child that exits on
/// `ScopeClosing` rather than holding the drain for its renewal interval.
#[cfg(feature = "dns01")]
#[test]
fn dns01_renewal_is_scope_owned_and_exits_on_closing() {
    let (cert_pem, key_pem) = crate::common::generate_self_signed_cert();
    let store = camber::CertStore::new(crate::common::certified_key_from_pem(&cert_pem, &key_pem));

    let (proof, ()) = prove_scope_owned(
        BOUND,
        |builder| builder,
        move |_| {
            camber::runtime_test_support::admit_dns01_renewal_for_test(store, ScriptedDnsProvider)
                .unwrap();
        },
    );

    proof.assert_owned("the dns01 renewal loop", RUNTIME_OWNED_CHILDREN + 2);
}
