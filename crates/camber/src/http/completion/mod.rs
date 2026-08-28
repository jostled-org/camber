//! The one account an admitted HTTP operation owes at its true terminal.
//!
//! Two files and no logic of its own. [`facts`] holds the orthogonal dimensions
//! every owner of one operation writes into, each once; [`finalizer`] holds the
//! single owner that reads them at the end of the response lifetime and writes
//! the operation's one record.
//!
//! The split is the contract. Many owners can name a fact about how a request
//! ended, and exactly one owner may say that it ended.

mod facts;
mod finalizer;

/// The WebSocket bridge is the one owner outside this module that renders an
/// optional dimension of its own, so the label helper crosses the boundary only
/// when that bridge is compiled.
#[cfg(feature = "ws")]
pub(in crate::http) use facts::optional_label;
pub(in crate::http) use facts::{
    ABSENT as ABSENT_LABEL, CompletionAccount, CompletionSnapshot, ConnectionEnd, DeliveryOutcome,
    ShutdownObservation, Telemetry, optional_vocabulary,
};
pub(in crate::http) use finalizer::OperationFinalizer;
