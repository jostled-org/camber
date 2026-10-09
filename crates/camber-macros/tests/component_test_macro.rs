use proc_macro2::{LineColumn, TokenStream};
use quote::quote;

#[path = "../src/expand.rs"]
mod expand;

#[test]
fn expansion_uses_the_standard_runtime_path_and_preserves_the_function() {
    let input = syn::parse_quote! {
        pub async fn ordinary_test() { work().await; }
    };
    let actual = expand::expand_test(TokenStream::new(), input);
    let expected = quote! {
        #[test]
        pub fn ordinary_test() {
            match ::camber::runtime::__test_async(|| async move { work().await; }) {
                ::core::result::Result::Ok(()) => {}
                ::core::result::Result::Err(error) => {
                    ::core::panic!("camber test runtime failed: {:?}", error);
                }
            }
        }
    };
    assert_eq!(actual.to_string(), expected.to_string());
    syn::parse2::<syn::ItemFn>(actual).expect("expansion is a function");
}

#[test]
fn expansion_preserves_test_attributes() {
    let input = syn::parse_quote! {
        #[should_panic(expected = "intentional panic")]
        #[ignore = "explicit opt-in"]
        async fn attributed_test() {}
    };
    let output = expand::expand_test(TokenStream::new(), input);
    let function = syn::parse2::<syn::ItemFn>(output).expect("expansion is a function");
    let attributes = &function.attrs;
    assert_eq!(
        quote!(#(#attributes)*).to_string(),
        quote! {
            #[test]
            #[should_panic(expected = "intentional panic")]
            #[ignore = "explicit opt-in"]
        }
        .to_string()
    );
}

fn diagnostics(arguments: TokenStream, source: &str) -> Vec<(String, LineColumn)> {
    let input = syn::parse_str(source).expect("test input parses");
    let output = expand::expand_test(arguments, input);
    let file = syn::parse2::<syn::File>(output).expect("diagnostics parse");
    file.items
        .into_iter()
        .map(|item| {
            let item = match item {
                syn::Item::Macro(item) => item,
                _ => panic!("expected compile_error"),
            };
            let diagnostic = item.mac.path.segments.last().expect("diagnostic path");
            assert_eq!(diagnostic.ident, "compile_error");
            let message = syn::parse2::<syn::LitStr>(item.mac.tokens).expect("diagnostic message");
            (message.value(), diagnostic.ident.span().start())
        })
        .collect()
}

#[test]
fn unsupported_arguments_report_the_argument_span() {
    let arguments = "\nflavor = \"current_thread\"".parse().unwrap();
    assert_eq!(
        diagnostics(arguments, "async fn test() {}"),
        vec![(
            "camber::test does not accept attribute arguments".into(),
            LineColumn { line: 2, column: 0 },
        )]
    );
}

#[test]
fn invalid_signatures_report_each_invalid_part() {
    let cases = [
        ("fn test() {}", "fn", "requires an async fn"),
        (
            "async fn test(value: u8) {}",
            "value",
            "does not support parameters",
        ),
        (
            "async fn test<T>() {}",
            "T",
            "does not support generic parameters",
        ),
        (
            "async fn test() -> () {}",
            "->",
            "does not support an explicit return type",
        ),
        (
            "async unsafe fn test() {}",
            "unsafe",
            "does not support unsafe functions",
        ),
        (
            "const async fn test() {}",
            "const",
            "does not support const functions",
        ),
        (
            "async extern \"C\" fn test() {}",
            "extern",
            "does not support an explicit ABI",
        ),
        (
            "async fn test() where (): Sized {}",
            "where",
            "does not support a where clause",
        ),
        (
            "async fn test(...) {}",
            "...",
            "does not support variadic functions",
        ),
    ];
    for (source, token, message) in cases {
        assert_eq!(
            diagnostics(TokenStream::new(), source),
            vec![(
                format!("camber::test {message}"),
                LineColumn {
                    line: 1,
                    column: source.find(token).unwrap()
                },
            )],
            "{source}"
        );
    }
}

#[test]
fn invalid_signatures_accumulate_independent_diagnostics() {
    let errors = diagnostics(TokenStream::new(), "fn test<T>(value: T) -> T { value }");
    assert_eq!(errors.len(), 4);
    assert_eq!(
        errors
            .iter()
            .map(|(message, _)| message.as_str())
            .collect::<Vec<_>>(),
        [
            "camber::test requires an async fn",
            "camber::test does not support parameters",
            "camber::test does not support generic parameters",
            "camber::test does not support an explicit return type",
        ]
    );
}
