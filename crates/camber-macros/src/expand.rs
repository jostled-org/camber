use proc_macro2::TokenStream;
use quote::quote;
use syn::{FnModifiers, ItemFn, ReturnType, Safety, Signature};

pub(crate) fn expand_test(arguments: TokenStream, input: ItemFn) -> TokenStream {
    let sig = &input.sig;
    let body = &input.block;
    let attrs = &input.attrs;
    let vis = &input.vis;
    let name = &sig.ident;

    let mut errors = validate_arguments(&arguments);
    validate_modifiers(&input.modifiers, &mut errors);
    validate_signature(sig, &mut errors);
    if let Some(errors) = errors {
        return errors.to_compile_error();
    }

    quote! {
        #[test]
        #(#attrs)*
        #vis fn #name() {
            match ::camber::runtime::__test_async(|| async move #body) {
                ::core::result::Result::Ok(()) => {}
                ::core::result::Result::Err(error) => {
                    ::core::panic!("camber test runtime failed: {:?}", error);
                }
            }
        }
    }
}

fn validate_modifiers(modifiers: &FnModifiers, errors: &mut Option<syn::Error>) {
    match modifiers.require_empty() {
        Ok(()) => {}
        Err(error) => push_error(errors, error),
    }
}

fn validate_arguments(arguments: &TokenStream) -> Option<syn::Error> {
    match arguments.is_empty() {
        true => None,
        false => Some(syn::Error::new_spanned(
            arguments,
            "camber::test does not accept attribute arguments",
        )),
    }
}

fn validate_signature(signature: &Signature, errors: &mut Option<syn::Error>) {
    match signature.asyncness {
        Some(_) => {}
        None => push_error(
            errors,
            syn::Error::new_spanned(signature.fn_token, "camber::test requires an async fn"),
        ),
    }

    match signature.inputs.is_empty() {
        true => {}
        false => push_error(
            errors,
            syn::Error::new_spanned(
                &signature.inputs,
                "camber::test does not support parameters",
            ),
        ),
    }

    match signature.generics.params.is_empty() {
        true => {}
        false => push_error(
            errors,
            syn::Error::new_spanned(
                &signature.generics.params,
                "camber::test does not support generic parameters",
            ),
        ),
    }

    if let ReturnType::Type(arrow, output) = &signature.output {
        push_error(
            errors,
            syn::Error::new_spanned(
                quote! { #arrow #output },
                "camber::test does not support an explicit return type",
            ),
        );
    }

    match &signature.safety {
        Safety::Unsafe(unsafety) => push_error(
            errors,
            syn::Error::new_spanned(unsafety, "camber::test does not support unsafe functions"),
        ),
        Safety::Safe(_) | Safety::Default => {}
    }

    if let Some(constness) = &signature.constness {
        push_error(
            errors,
            syn::Error::new_spanned(constness, "camber::test does not support const functions"),
        );
    }

    if let Some(abi) = &signature.abi {
        push_error(
            errors,
            syn::Error::new_spanned(abi, "camber::test does not support an explicit ABI"),
        );
    }

    if let Some(where_clause) = &signature.generics.where_clause {
        push_error(
            errors,
            syn::Error::new_spanned(where_clause, "camber::test does not support a where clause"),
        );
    }

    if let Some(variadic) = &signature.variadic {
        push_error(
            errors,
            syn::Error::new_spanned(variadic, "camber::test does not support variadic functions"),
        );
    }
}

fn push_error(errors: &mut Option<syn::Error>, error: syn::Error) {
    match errors {
        Some(errors) => errors.combine(error),
        None => *errors = Some(error),
    }
}
