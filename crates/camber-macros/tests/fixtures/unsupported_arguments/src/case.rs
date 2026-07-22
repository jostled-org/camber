extern crate self as camber;

#[path = "../../support/runtime.rs"]
pub mod runtime;

#[renamed_macros::test(worker_threads = 2)]
async fn arguments_are_not_supported() {}
