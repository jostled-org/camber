pub mod common;

// The integration terminal rows run only in the full-feature build. The DNS
// peer module supplies the controlled ACME and Cloudflare-shaped peers and the
// renewal clock helpers.
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/dns_cache_files.rs"]
pub mod dns_cache_files;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/dns_cleanup_peers.rs"]
pub mod dns_cleanup_peers;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/integration_events.rs"]
pub mod integration_events;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/integration_rows.rs"]
pub mod integration_rows;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/integration_vocabulary.rs"]
pub mod integration_vocabulary;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/nats_ack_rows.rs"]
pub mod nats_ack_rows;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/nats_peer.rs"]
pub mod nats_peer;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/scripted_peer.rs"]
pub mod scripted_peer;
#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/sqs_peer.rs"]
pub mod sqs_peer;

#[path = "process_observability_globals/install_once.rs"]
mod install_once;

#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "process_observability_globals/event_rows.rs"]
mod event_rows;

#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "process_observability_globals/nats_ack_events.rs"]
mod nats_ack_events;

#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "process_observability_globals/nats_events.rs"]
mod nats_events;

#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "process_observability_globals/sqs_events.rs"]
mod sqs_events;

#[cfg(all(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "process_observability_globals/dns_events.rs"]
mod dns_events;

#[cfg(feature = "otel")]
#[path = "process_observability_globals/otlp_export.rs"]
mod otlp_export;
