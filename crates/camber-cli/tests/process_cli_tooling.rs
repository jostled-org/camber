#[path = "process_cli_tooling/support.rs"]
pub mod support;

#[path = "process_cli_tooling/configuration.rs"]
mod configuration;
#[path = "process_cli_tooling/context.rs"]
mod context;
#[path = "external_cli_operational/docker.rs"]
pub mod docker_support;
#[path = "process_cli_tooling/external_contracts.rs"]
mod external_contracts;
#[path = "process_cli_tooling/scaffolding.rs"]
mod scaffolding;

#[path = "external_cli_operational/resources.rs"]
pub mod resources;
