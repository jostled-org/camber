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
#[path = "focused_api_contracts/framework_rejections.rs"]
mod framework_rejections;
#[path = "focused_api_contracts/owned_server_api.rs"]
mod owned_server_api;
#[path = "focused_api_contracts/public_trait_contracts.rs"]
mod public_trait_contracts;
#[path = "focused_api_contracts/published_compatibility.rs"]
mod published_compatibility;
#[path = "focused_api_contracts/query_identity.rs"]
mod query_identity;
#[path = "focused_api_contracts/request_validation.rs"]
mod request_validation;
#[path = "focused_api_contracts/response_validation.rs"]
mod response_validation;
#[path = "focused_api_contracts/runtime_configuration.rs"]
mod runtime_configuration;
#[path = "focused_api_contracts/runtime_results.rs"]
mod runtime_results;
