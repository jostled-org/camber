pub mod common;

// The closed lifecycle vocabulary the aggregate rows read their returned
// account through. Mounted beside `common` for the same reason the other roots
// that read it do: it reaches no other support module.
#[path = "support/lifecycle_kinds.rs"]
pub mod lifecycle_kinds;

// Keep this as a textual include so the child-process filter remains the exact
// root-level `lifecycle_signal_child` after the old root is removed.
include!("acceptance_owned_lifecycle/owned_server_lifecycle.rs");

// Fixture contracts belong to this final root alone. Keep `common` focused on
// helper exports so importing it cannot register tests in another binary.
use common::*;
#[path = "support/fixture_contracts.rs"]
mod fixture_contracts;

#[path = "acceptance_owned_lifecycle/background_serving.rs"]
mod background_serving;
#[cfg(feature = "ws")]
#[path = "acceptance_owned_lifecycle/connection_ownership.rs"]
mod connection_ownership;
#[path = "acceptance_owned_lifecycle/direct_serving.rs"]
mod direct_serving;
#[path = "acceptance_owned_lifecycle/disconnect/mod.rs"]
mod disconnect;
#[path = "acceptance_owned_lifecycle/dns_admission.rs"]
mod dns_admission;
#[cfg(feature = "dns01")]
#[path = "support/dns_cache_files.rs"]
pub mod dns_cache_files;
#[path = "acceptance_owned_lifecycle/dns_cleanup.rs"]
mod dns_cleanup;
#[cfg(feature = "dns01")]
#[path = "support/dns_cleanup_peers.rs"]
pub mod dns_cleanup_peers;
#[path = "acceptance_owned_lifecycle/dns_renewal.rs"]
mod dns_renewal;
#[path = "acceptance_owned_lifecycle/framework_rejections.rs"]
mod framework_rejections;
#[cfg(feature = "grpc")]
#[path = "support/grpc_forms.rs"]
pub mod grpc_forms;
#[cfg(feature = "grpc")]
#[path = "support/grpc_rows.rs"]
pub mod grpc_rows;
#[cfg(feature = "grpc")]
#[path = "acceptance_owned_lifecycle/grpc_shutdown.rs"]
mod grpc_shutdown;
#[cfg(feature = "grpc")]
#[path = "acceptance_owned_lifecycle/grpc_transfer_bounds.rs"]
mod grpc_transfer_bounds;
#[path = "acceptance_owned_lifecycle/integration_registry.rs"]
mod integration_registry;
#[path = "support/integration_rows.rs"]
pub mod integration_rows;
#[path = "acceptance_owned_lifecycle/lifecycle_aggregate.rs"]
mod lifecycle_aggregate;
#[cfg(feature = "ws")]
#[path = "acceptance_owned_lifecycle/lifecycle_test_support.rs"]
mod lifecycle_test_support;
#[path = "acceptance_owned_lifecycle/nats_accounts.rs"]
mod nats_accounts;
#[cfg(feature = "nats")]
#[path = "support/nats_ack_rows.rs"]
pub mod nats_ack_rows;
#[path = "acceptance_owned_lifecycle/nats_acknowledged.rs"]
mod nats_acknowledged;
#[cfg(feature = "nats")]
#[path = "support/nats_peer.rs"]
pub mod nats_peer;
#[path = "acceptance_owned_lifecycle/scope_drain.rs"]
mod scope_drain;
// Every build: the integration registry rows share its poison-tolerant lock.
#[path = "support/scripted_peer.rs"]
pub mod scripted_peer;
#[path = "acceptance_owned_lifecycle/serve_variants.rs"]
mod serve_variants;
#[path = "acceptance_owned_lifecycle/server_stop_causality.rs"]
mod server_stop_causality;
#[path = "acceptance_owned_lifecycle/service_budgets.rs"]
mod service_budgets;
#[cfg(feature = "ws")]
#[path = "acceptance_owned_lifecycle/shared_binary_payloads.rs"]
mod shared_binary_payloads;
#[path = "acceptance_owned_lifecycle/sqs_accounts.rs"]
mod sqs_accounts;
#[path = "acceptance_owned_lifecycle/sqs_close.rs"]
mod sqs_close;
#[cfg(feature = "sqs")]
#[path = "support/sqs_peer.rs"]
pub mod sqs_peer;
#[cfg(feature = "ws")]
#[path = "acceptance_owned_lifecycle/websocket_callback_ownership.rs"]
mod websocket_callback_ownership;
#[cfg(feature = "ws")]
#[path = "acceptance_owned_lifecycle/websocket_directions.rs"]
mod websocket_directions;
