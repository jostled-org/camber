//! One standalone probe for the removed `LifecycleParticipant::Server` variant.
//!
//! It constructs that one variant and nothing else, so the diagnostic it
//! produces after the removal names `Server` alone.

use camber::LifecycleParticipant;

fn main() {
    let _owner = LifecycleParticipant::Server;
}
