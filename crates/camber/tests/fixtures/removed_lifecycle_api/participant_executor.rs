//! One standalone probe for the removed `LifecycleParticipant::Executor`
//! variant.
//!
//! It constructs that one variant and nothing else, so the diagnostic it
//! produces after the removal names `Executor` alone.

use camber::LifecycleParticipant;

fn main() {
    let _owner = LifecycleParticipant::Executor;
}
