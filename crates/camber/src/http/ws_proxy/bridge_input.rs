//! What one direct bridge is launched with.
//!
//! The upgrade registration builds it, and the direct bridge takes it apart.

use super::super::Request;
use super::super::mock::LifecycleScript;
use super::super::server_lifecycle::ConnectionPermit;
use super::ownership::BridgeAttachment;
use std::sync::Arc;

/// Everything one direct bridge owns once its upgrade is registered.
///
/// Built once, by the registration, and moved whole into the one launch that
/// spends it. Each value has that single owner: the permit and the script are
/// the connection's existing shared handles, not copies made for the bridge.
pub(in crate::http) struct WsBridgeInput {
    pub(super) on_upgrade: hyper::upgrade::OnUpgrade,
    pub(super) req: Request,
    pub(super) buffer_size: usize,
    pub(super) attachment: BridgeAttachment,
    pub(super) script: Option<Arc<LifecycleScript>>,
    pub(super) permit: Arc<ConnectionPermit>,
}
