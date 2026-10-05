#[cfg(feature = "dns01")]
#[path = "support/challenge_dns.rs"]
pub mod challenge_dns;
#[cfg(feature = "dns01")]
#[path = "support/dns_cache_files.rs"]
pub mod dns_cache_files;
#[cfg(feature = "dns01")]
#[path = "support/dns_cleanup_peers.rs"]
pub mod dns_cleanup_peers;
#[path = "support/http.rs"]
pub mod http;
#[path = "support/integration_rows.rs"]
pub mod integration_rows;
#[cfg(feature = "dns01")]
#[path = "support/local_integrations/readiness.rs"]
pub mod local_readiness;
#[path = "support/runtime.rs"]
pub mod runtime_support;
#[path = "support/scripted_peer.rs"]
pub mod scripted_peer;
#[path = "support/tcp_relay.rs"]
pub mod tcp_relay;

pub mod common {
    pub use crate::runtime_support::*;
}

#[cfg(feature = "nats")]
#[path = "external_feature_services/jetstream_streams.rs"]
pub mod jetstream_streams;
#[path = "external_feature_services/resources.rs"]
pub mod resources;

#[path = "external_feature_services/live_dns01.rs"]
mod external_dns01;
#[path = "external_feature_services/live_nats.rs"]
mod external_nats;
#[path = "external_feature_services/live_sqs.rs"]
mod external_sqs;
#[path = "external_feature_services/local_dns01.rs"]
mod local_dns01;
