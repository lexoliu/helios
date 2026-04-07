use proc_macro::TokenStream;
use quote::quote;
use syn::{FnArg, ItemFn, Result, ReturnType, parse_macro_input};

#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "`#[helios_api::main]` does not accept arguments",
        )
        .into_compile_error()
        .into();
    }

    let function = parse_macro_input!(item as ItemFn);
    expand_main(function)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_main(function: ItemFn) -> Result<proc_macro2::TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            function.sig.fn_token,
            "`#[helios_api::main]` requires `async fn`",
        ));
    }

    if function.sig.ident != "main" {
        return Err(syn::Error::new_spanned(
            &function.sig.ident,
            "`#[helios_api::main]` must be attached to `async fn main`",
        ));
    }

    if !function.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &function.sig.generics,
            "`#[helios_api::main]` does not support generic functions",
        ));
    }

    if let Some(input) = function.sig.inputs.first() {
        match input {
            FnArg::Receiver(receiver) => {
                return Err(syn::Error::new_spanned(
                    receiver,
                    "`#[helios_api::main]` does not accept a receiver",
                ));
            }
            FnArg::Typed(argument) => {
                return Err(syn::Error::new_spanned(
                    &argument.pat,
                    "`#[helios_api::main]` does not accept arguments",
                ));
            }
        }
    }

    let output_type = match &function.sig.output {
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &function.sig.output,
                "`#[helios_api::main]` requires an explicit return type",
            ));
        }
        ReturnType::Type(_, ty) => quote!(#ty),
    };
    let function_name = &function.sig.ident;

    Ok(quote! {
        #function

        mod bindings {
            pub use ::helios_api::bindings::*;
        }

        struct __HeliosProgram;

        ::helios_api::bindings::export!(__HeliosProgram);

        impl ::helios_api::bindings::exports::wasi::cli::run::Guest for __HeliosProgram {
            async fn run() -> core::result::Result<(), ()> {
                <#output_type as ::helios_api::MainOutput>::into_run_result(#function_name().await)
            }
        }
    })
}
