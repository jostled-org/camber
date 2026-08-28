//! One standalone control for the broad `CheckpointWaitProbe` type.
//!
//! Step 11 leaves this type in place, so this probe compiles here and is the
//! control that keeps the removal oracle honest. Step 12 removes the type and
//! this probe becomes the removal evidence for it, with its body unchanged.

use camber::http::mock::CheckpointWaitProbe;

fn main() {
    let _size = size_of::<CheckpointWaitProbe>();
}
