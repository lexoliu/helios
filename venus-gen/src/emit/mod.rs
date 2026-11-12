//! The Rust emitters: from the analysed registry to the source of the
//! `helios-venus` protocol module.

mod funcs;
mod items;
mod names;
mod parity;
mod wire;

use proc_macro2::TokenStream;
use quote::quote;

use crate::analysis::Analysis;
use crate::registry::{Category, RegistryError};
use wire::Wire;

pub use items::{TypeSet, type_set};

/// Why the generated tokens could not become source text.
#[derive(Debug, thiserror::Error)]
pub enum EmitError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("generated code does not parse as Rust: {message}")]
    Syntax { message: String },
}

/// The source of the protocol module: types, wire walks and commands.
pub fn protocol_source(an: &Analysis) -> Result<String, EmitError> {
    let tokens = protocol_tokens(an)?;
    format_tokens(tokens)
}

/// The source of the parity test's support module: every generated command
/// behind a uniform call (see `parity.rs`).
pub fn parity_source(an: &Analysis) -> Result<String, EmitError> {
    format_tokens(parity::parity_tokens(an)?)
}

fn format_tokens(tokens: TokenStream) -> Result<String, EmitError> {
    let file: syn::File = syn::parse2(tokens).map_err(|error| EmitError::Syntax {
        message: error.to_string(),
    })?;
    Ok(prettyplease::unparse(&file))
}

pub fn protocol_tokens(an: &Analysis) -> Result<TokenStream, RegistryError> {
    let set = type_set(an)?;
    let wire = Wire::new(an);
    let scalars = items::scalar_items(an)?;
    let handles = items::handle_items(an);
    let aggregates = items::aggregate_items(an, &set)?;
    let assertions = items::layout_assertions(an, &set)?;
    let info = items::info_items(an)?;

    let mut functions = TokenStream::new();
    for &id in an
        .supported
        .get(&Category::Handle)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        functions.extend(wire.handle_functions(id)?);
    }
    for &id in &an.generated_structs {
        if matches!(
            an.reg.types[id].name.as_str(),
            "VkBaseInStructure" | "VkBaseOutStructure"
        ) {
            continue;
        }
        functions.extend(wire.type_functions(id)?);
    }
    for &id in &an.generated_commands {
        functions.extend(wire.command_functions(id)?);
    }
    Ok(quote! {
        #scalars
        #handles
        #aggregates
        #assertions
        #info
        #functions
    })
}
