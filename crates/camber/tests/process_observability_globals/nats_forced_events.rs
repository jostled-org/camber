//! Force the reply receiver to end before its waiting publish is aborted.
//!
//! Forced teardown aborts async children in admission order. The case names
//! the close owner, which holds the reply receiver and is admitted at connect,
//! and requires the first abort to be the one that ends it.

use crate::common::{ArmedWatch, observe_armed_sequence};
use crate::event_rows::failed_as;
use crate::integration_events::{Observation, success};
use crate::integration_rows::{
    ROW_BOUND, Row, all, clean_run, expect, expect_eq, expect_refused, row_bounded, settled,
    unknown,
};
use crate::nats_ack_rows::{
    FORCED_GRACE, SUBJECT, ack_probe, acknowledged, expect_forced_accounts, expect_withheld,
    wait_snapshot,
};
use crate::nats_peer::{NatsPeer, PeerControl};
use camber::mq::nats::Connection;
use camber::runtime_test_support::{AdmittedScope, NatsAckReceiver, RuntimeCheckpoint};
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, runtime};
use std::pin::pin;
use std::sync::mpsc;
use std::time::Duration;

const ABORTED: RuntimeCheckpoint = RuntimeCheckpoint::ScopeChildAbortRequested;

pub(crate) fn receiver_force_precedes_publish_abort() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let (connection_tx, connection_rx) = mpsc::channel();
    let (pending, pending_rx) = mpsc::channel();
    let observation = Observation::start();
    let (teardown, observed) = observe_armed_sequence(
        |builder| builder.shutdown_timeout(FORCED_GRACE),
        move |gate| -> Row {
            // Connect admits exactly one root-scope child: the close owner.
            let close_owner = gate.controller().scope_settlement().name_next_admission();
            let connection = settled("connect", builder.connect())?;
            connection_tx
                .send((connection, close_owner))
                .map_err(|error| error.to_string())?;
            pending_rx
                .recv_timeout(ROW_BOUND)
                .map_err(|error| error.to_string())?;
            gate.arm(ABORTED);
            Ok(())
        },
        move |watch| -> Row {
            let (connection, close_owner) = connection_rx
                .recv_timeout(ROW_BOUND)
                .map_err(|error| error.to_string())?;
            clean_run(
                runtime::builder()
                    .run(|| read_forced_publish(connection, &close_owner, control, pending, watch)),
            )
        },
    );
    let events = observation.finish();
    let nats = IntegrationKind::Nats;
    let verdict = all([
        observed,
        expect_forced_accounts(
            &teardown.map(|_| ()),
            Some(unknown(IntegrationOperation::Publish)),
        ),
        events.expect_terminals(&[
            success(nats, IntegrationOperation::Connect),
            failed_as(nats, unknown(IntegrationOperation::Publish)).under_shutdown(),
            success(nats, IntegrationOperation::Close)
                .or(IntegrationFailure::Timeout)
                .under_shutdown(),
        ]),
        events.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Poll the named close owner until it leaves the root scope.
fn wait_close_owner_settled(close_owner: &AdmittedScope) -> Row {
    row_bounded("the aborted close owner to leave the scope", async {
        loop {
            match close_owner.settled() {
                Ok(true) => return Ok(()),
                Ok(false) => tokio::time::sleep(Duration::from_millis(1)).await,
                Err(error) => return Err(error.to_string()),
            }
        }
    })?
}

fn read_forced_publish(
    connection: Connection,
    close_owner: &AdmittedScope,
    control: PeerControl,
    pending: mpsc::Sender<()>,
    watch: &ArmedWatch<'_>,
) -> Row {
    let probe = ack_probe(&connection)?;
    let mut publish = pin!(connection.publish(SUBJECT, b"forced"));
    runtime::block_on(expect_withheld(
        "foreign publish",
        &control,
        0,
        publish.as_mut(),
    ))?;
    expect(
        "the connect admitted the close owner",
        close_owner.admitted().map_err(|error| error.to_string())?,
    )?;
    expect(
        "the close owner is live while the publish waits",
        !close_owner.settled().map_err(|error| error.to_string())?,
    )?;
    pending.send(()).map_err(|error| error.to_string())?;
    watch.wait_armed_within(ROW_BOUND);
    watch
        .probe(ABORTED, |_| {
            wait_close_owner_settled(close_owner)?;
            let snapshot = wait_snapshot("the aborted reply receiver", &probe, |snapshot| {
                snapshot.receiver == NatsAckReceiver::Ended
            })?;
            expect_eq("pending correlation entries", snapshot.pending, 0)?;
            expect("foreign runtime is stopping", !runtime::is_shutting_down())?;
            expect_refused(
                "publish settled before its own abort",
                row_bounded("publish", publish)?,
                unknown(IntegrationOperation::Publish),
            )?;
            expect(
                "foreign runtime stopped while reading",
                !runtime::is_shutting_down(),
            )
        })
        .ok_or_else(|| "forced child abort checkpoint was not reached".to_owned())?
}
