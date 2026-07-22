extern crate self as camber;

#[path = "../../support/runtime.rs"]
pub mod runtime;

#[renamed_macros::test]
async fn with_parameter(value: usize) {}

#[renamed_macros::test]
async fn with_generic_parameter<T>() {}

#[renamed_macros::test]
async fn with_return_type() -> usize {
    42
}

#[renamed_macros::test]
async unsafe fn unsafe_function() {}

#[renamed_macros::test]
const async fn const_function() {}

#[renamed_macros::test]
async extern "C" fn explicit_abi() {}

#[renamed_macros::test]
async fn with_where_clause()
where
    (): Copy,
{
}
