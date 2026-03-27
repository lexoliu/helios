use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{FnArg, Ident, ItemFn, LitStr, Result, ReturnType, Token, parse_macro_input};

struct MainInput {
    path: LitStr,
    world: LitStr,
}

impl Parse for MainInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut path = None;
        let mut world = None;

        while !input.is_empty() {
            let field: Ident = input.parse()?;
            input.parse::<Token![:]>()?;

            match field.to_string().as_str() {
                "path" => path = Some(input.parse()?),
                "world" => world = Some(input.parse()?),
                other => {
                    return Err(syn::Error::new(
                        field.span(),
                        format!("unsupported helios program field {other:?}"),
                    ));
                }
            }

            if input.is_empty() {
                break;
            }

            input.parse::<Token![,]>()?;
        }

        Ok(Self {
            path: path.ok_or_else(|| input.error("missing required field `path`"))?,
            world: world.ok_or_else(|| input.error("missing required field `world`"))?,
        })
    }
}

#[proc_macro_attribute]
pub fn main(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(attr as MainInput);
    let function = parse_macro_input!(item as ItemFn);
    expand_main(input, function)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand_main(input: MainInput, function: ItemFn) -> Result<proc_macro2::TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &function.sig.fn_token,
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

    for input in &function.sig.inputs {
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

    let path = input.path;
    let world = input.world;
    let output = match &function.sig.output {
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &function.sig.output,
                "`#[helios_api::main]` requires an explicit return type",
            ));
        }
        ReturnType::Type(_, ty) => quote!(-> #ty),
    };
    let function_name = &function.sig.ident;

    Ok(quote! {
        pub mod bindings {
            use ::helios_api::wit_bindgen as wit_bindgen;

            ::helios_api::wit_bindgen::generate!({
                path: #path,
                world: #world,
                generate_all,
                default_bindings_module: "bindings",
                pub_export_macro: true,
            });
        }

        #function

        struct __HeliosProgram;

        bindings::export!(__HeliosProgram);

        impl bindings::exports::wasi::cli::run::Guest for __HeliosProgram {
            async fn run() #output {
                #function_name().await
            }
        }
    })
}
