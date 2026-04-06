mod expand;

use proc_macro::TokenStream;
use syn::{ItemFn, parse_macro_input};

/// Marks an async function as a Camber test.
///
/// Sets up a multi-thread Tokio runtime with Camber context installed.
/// The test body runs as an async block inside `camber::runtime::__test_async`.
///
/// ```ignore
/// #[camber::test]
/// async fn my_test() {
///     let handle = camber::spawn_async(async { 42 });
///     assert_eq!(handle.await.unwrap(), 42);
/// }
/// ```
#[proc_macro_attribute]
pub fn test(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);
    expand::expand_test(input).into()
}
