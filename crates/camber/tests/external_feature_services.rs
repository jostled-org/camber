#[path = "support/http.rs"]
pub mod http;
#[path = "support/runtime.rs"]
pub mod runtime_support;

pub mod common {
    pub use crate::runtime_support::*;
}

#[path = "external_feature_services/resources.rs"]
pub mod resources;

#[path = "external_feature_services/live_dns01.rs"]
mod external_dns01;
#[path = "external_feature_services/live_nats.rs"]
mod external_nats;
