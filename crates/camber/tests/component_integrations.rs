#[path = "support/delivery_fixture.rs"]
pub mod delivery_fixture;
#[cfg(feature = "dns01")]
#[path = "support/dns_cache_files.rs"]
pub mod dns_cache_files;
#[cfg(feature = "dns01")]
#[path = "support/dns_cleanup_peers.rs"]
pub mod dns_cleanup_peers;
// Mounted for the bounded poll `process` and the local fixtures wait through.
#[path = "support/http.rs"]
pub mod http;
#[path = "support/integration_rows.rs"]
pub mod integration_rows;
#[path = "support/local_integrations/mod.rs"]
pub mod local_integrations;
#[cfg(feature = "nats")]
#[path = "support/nats_ack_rows.rs"]
pub mod nats_ack_rows;
#[cfg(feature = "nats")]
#[path = "support/nats_peer.rs"]
pub mod nats_peer;
#[path = "support/process.rs"]
pub mod process;
#[cfg(any(feature = "dns01", feature = "nats", feature = "sqs"))]
#[path = "support/scripted_peer.rs"]
pub mod scripted_peer;
#[cfg(feature = "sqs")]
#[path = "support/sqs_peer.rs"]
pub mod sqs_peer;
#[path = "support/temp.rs"]
pub mod temp_support;

#[path = "component_integrations/acme_configuration.rs"]
mod acme_configuration;
#[cfg(all(feature = "acme", feature = "dns01"))]
#[path = "component_integrations/acme_validation_rows.rs"]
mod acme_validation_rows;
#[path = "component_integrations/automatic_tls_cache.rs"]
mod automatic_tls_cache;
#[cfg(feature = "dns01")]
#[path = "component_integrations/cloudflare_provider_simulation.rs"]
mod cloudflare_provider_simulation;
#[path = "component_integrations/dns_certificate_cache.rs"]
mod dns_certificate_cache;
#[path = "component_integrations/dns_cleanup.rs"]
mod dns_cleanup;
#[path = "component_integrations/dns_provider_and_cache.rs"]
mod dns_provider_and_cache;
#[path = "component_integrations/dns_renewal.rs"]
mod dns_renewal;
#[path = "component_integrations/dns_transport.rs"]
mod dns_transport;
#[path = "component_integrations/external_resource_contracts.rs"]
mod external_resource_contracts;
#[path = "component_integrations/local_fixture_lifecycle.rs"]
mod local_fixture_lifecycle;
#[path = "component_integrations/message_queue_validation.rs"]
mod message_queue_validation;
#[path = "component_integrations/nats_acknowledged.rs"]
mod nats_acknowledged;
#[path = "component_integrations/nats_operations.rs"]
mod nats_operations;
#[path = "component_integrations/nats_reply_fixture.rs"]
mod nats_reply_fixture;
#[path = "component_integrations/sqs_operations.rs"]
mod sqs_operations;

#[path = "external_feature_services/resources.rs"]
pub mod resources;
