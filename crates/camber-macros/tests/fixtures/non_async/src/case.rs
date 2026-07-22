extern crate self as camber;

#[path = "../../support/runtime.rs"]
pub mod runtime;

#[renamed_macros::test]
fn synchronous_function_is_invalid() {}
