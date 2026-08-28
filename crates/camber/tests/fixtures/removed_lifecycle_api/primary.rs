//! One standalone probe for the removed `LifecycleFailures::primary` accessor.
//!
//! It names that one accessor and nothing else, so the diagnostic it produces
//! after the removal is about `primary` rather than about whatever else a
//! larger probe happened to reach on the way there.

use camber::LifecycleFailures;

fn read(failures: &LifecycleFailures) {
    let _primary = failures.primary();
}

fn main() {
    let _read = read;
}
