//! One standalone probe for the removed `LifecycleParticipant::Upgrade`
//! variant.
//!
//! It constructs that one variant and nothing else, so the diagnostic it
//! produces after the removal names `Upgrade` alone.

use camber::LifecycleParticipant;

fn main() {
    let _owner = LifecycleParticipant::Upgrade;
}
