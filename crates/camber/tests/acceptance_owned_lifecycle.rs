pub mod common;

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
#[path = "acceptance_owned_lifecycle/direct_serving.rs"]
mod direct_serving;
#[path = "acceptance_owned_lifecycle/disconnect/mod.rs"]
mod disconnect;
#[path = "acceptance_owned_lifecycle/framework_rejections.rs"]
mod framework_rejections;
#[path = "acceptance_owned_lifecycle/scope_drain.rs"]
mod scope_drain;
#[path = "acceptance_owned_lifecycle/serve_variants.rs"]
mod serve_variants;
