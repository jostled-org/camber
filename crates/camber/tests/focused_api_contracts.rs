#[path = "support/delivery_fixture.rs"]
pub mod delivery_fixture;
#[path = "support/deterministic.rs"]
pub mod deterministic;
// Mounted for the bounded poll `process` reaps its children through, and for
// the head read and address-reuse check the ambiguous transport row in
// `retry_delays` uses.
#[path = "support/http.rs"]
pub mod http;
#[path = "support/integration_vocabulary.rs"]
pub mod integration_vocabulary;
#[path = "support/leaky_source.rs"]
pub mod leaky_source;
#[path = "support/lifecycle_kinds.rs"]
pub mod lifecycle_kinds;
#[path = "support/local_integrations/mod.rs"]
pub mod local_integrations;
#[path = "support/process.rs"]
pub mod process;
#[path = "support/rejection_kinds.rs"]
pub mod rejection_kinds;
#[path = "support/temp.rs"]
pub mod temp_support;
#[path = "support/tls.rs"]
pub mod tls_support;

#[path = "focused_api_contracts/body_admission.rs"]
mod body_admission;
#[path = "focused_api_contracts/certificate_management.rs"]
mod certificate_management;
#[path = "focused_api_contracts/channel_errors.rs"]
mod channel_errors;
#[path = "focused_api_contracts/configuration_loading.rs"]
mod configuration_loading;
#[path = "focused_api_contracts/configuration_validation.rs"]
mod configuration_validation;
#[path = "focused_api_contracts/delivery_inputs.rs"]
mod delivery_inputs;
#[path = "focused_api_contracts/framework_rejections.rs"]
mod framework_rejections;
#[path = "focused_api_contracts/integration_api.rs"]
mod integration_api;
#[path = "focused_api_contracts/integration_delivery.rs"]
mod integration_delivery;
#[cfg(feature = "dns01")]
#[path = "focused_api_contracts/integration_dns_cutover.rs"]
mod integration_dns_cutover;
#[path = "focused_api_contracts/integration_errors.rs"]
mod integration_errors;
#[cfg(feature = "nats")]
#[path = "focused_api_contracts/integration_nats_cutover.rs"]
mod integration_nats_cutover;
#[cfg(feature = "sqs")]
#[path = "focused_api_contracts/integration_sqs_cutover.rs"]
mod integration_sqs_cutover;
#[path = "focused_api_contracts/local_integration_delivery.rs"]
mod local_integration_delivery;
#[cfg(feature = "nats")]
#[path = "focused_api_contracts/nats_acknowledged_api.rs"]
mod nats_acknowledged_api;
#[path = "focused_api_contracts/owned_server_api.rs"]
mod owned_server_api;
#[path = "focused_api_contracts/public_trait_contracts.rs"]
mod public_trait_contracts;
#[path = "focused_api_contracts/published_compatibility.rs"]
mod published_compatibility;
#[path = "focused_api_contracts/query_identity.rs"]
mod query_identity;
#[path = "focused_api_contracts/release_features.rs"]
mod release_features;
#[path = "focused_api_contracts/release_inputs.rs"]
mod release_inputs;
#[path = "focused_api_contracts/release_publishing.rs"]
mod release_publishing;
#[path = "focused_api_contracts/request_validation.rs"]
mod request_validation;
#[path = "focused_api_contracts/response_validation.rs"]
mod response_validation;
#[path = "focused_api_contracts/retry_delays.rs"]
mod retry_delays;
#[path = "focused_api_contracts/runtime_configuration.rs"]
mod runtime_configuration;
#[path = "focused_api_contracts/runtime_results.rs"]
mod runtime_results;
#[path = "focused_api_contracts/service_budgets.rs"]
mod service_budgets;
#[path = "focused_api_contracts/streaming_multipart.rs"]
mod streaming_multipart;
