use proc_macro2::TokenStream;
use quote::quote;
use syn::ItemFn;

pub(crate) fn expand_test(input: ItemFn) -> TokenStream {
    let sig = &input.sig;
    let body = &input.block;
    let attrs = &input.attrs;
    let vis = &input.vis;
    let name = &sig.ident;

    match sig.asyncness {
        Some(_) => {}
        None => {
            return syn::Error::new_spanned(sig.fn_token, "camber::test requires an async fn")
                .to_compile_error();
        }
    }

    quote! {
        #[test]
        #(#attrs)*
        #vis fn #name() {
            camber::runtime::__test_async(|| async move #body)
                .expect("camber test runtime failed")
        }
    }
}
