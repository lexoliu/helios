//! The test-support module the Mesa parity test compiles: one entry per
//! generated command that calls the command's functions with arguments given
//! as pointers to their storage, so a single loop can drive every command.
//!
//! The arguments of a call are `args[i]`, a pointer to the storage of the
//! i-th parameter (a static-array parameter's storage is the array itself).
//! Return values travel as up to eight bytes, which every serializable
//! return type fits in.

use proc_macro2::{Literal, TokenStream};
use quote::quote;

use super::names::{decl_type, func, ident};
use crate::analysis::Analysis;
use crate::registry::{RegistryError, TypeId};

/// The tokens of the parity support module's contents.
pub fn parity_tokens(an: &Analysis) -> Result<TokenStream, RegistryError> {
    let mut wrappers = TokenStream::new();
    let mut entries = Vec::new();
    for &id in &an.generated_commands {
        let (wrapper, entry) = command_entry(an, id)?;
        wrappers.extend(wrapper);
        entries.push(entry);
    }
    Ok(quote! {
        /// Up to eight bytes of a return value.
        pub type ReturnBytes = [u8; 8];

        /// One generated command, callable with arguments given as pointers
        /// to their storage.
        pub struct CommandEntry {
            pub name: &'static str,
            pub command_type: u32,
            /// The size in bytes of the command's return value, zero for a
            /// command that returns nothing.
            pub ret_size: usize,
            pub sizeof: unsafe fn(&RendererProtocol, &[*mut ::core::ffi::c_void])
                -> Result<usize, EncodeError>,
            pub encode: unsafe fn(&mut Encoder<'_>, u32, &[*mut ::core::ffi::c_void]),
            pub sizeof_reply: unsafe fn(&RendererProtocol, &[*mut ::core::ffi::c_void])
                -> Result<usize, EncodeError>,
            pub encode_reply:
                unsafe fn(&mut Encoder<'_>, ReturnBytes, &[*mut ::core::ffi::c_void]),
            pub decode_reply:
                unsafe fn(&mut Decoder<'_>, &[*mut ::core::ffi::c_void]) -> ReturnBytes,
        }

        #wrappers

        /// Every generated command, in registry order.
        pub static COMMANDS: &[CommandEntry] = &[#(#entries),*];
    })
}

fn command_entry(an: &Analysis, id: TypeId) -> Result<(TokenStream, TokenStream), RegistryError> {
    let ty = &an.reg.types[id];
    let name = ty.name.as_str();
    let command_const = ident(&format!("VK_COMMAND_TYPE_{name}_EXT"));

    let mut reads = Vec::new();
    for (index, var) in ty.variables.iter().enumerate() {
        let var_ty = decl_type(an, &var.ty, &var.name)?;
        let index = Literal::usize_unsuffixed(index);
        reads.push(quote!(::core::ptr::read(args[#index] as *const #var_ty)));
    }

    let ret_ty = match &ty.ret {
        Some(ret) => Some(decl_type(an, ret, "the return value")?),
        None => None,
    };
    let ret_size = match &ret_ty {
        Some(ret_ty) => quote!(::core::mem::size_of::<#ret_ty>()),
        None => quote!(0),
    };

    let sizeof_fn = func("sizeof", name);
    let encode_fn = func("encode", name);
    let sizeof_reply_fn = func("sizeof", &format!("{name}_reply"));
    let encode_reply_fn = func("encode", &format!("{name}_reply"));
    let decode_reply_fn = func("decode", &format!("{name}_reply"));

    let w_sizeof = ident(&format!("parity_sizeof_{name}"));
    let w_encode = ident(&format!("parity_encode_{name}"));
    let w_sizeof_reply = ident(&format!("parity_sizeof_reply_{name}"));
    let w_encode_reply = ident(&format!("parity_encode_reply_{name}"));
    let w_decode_reply = ident(&format!("parity_decode_reply_{name}"));

    let (ret_read, ret_arg) = match &ret_ty {
        Some(ret_ty) => (
            quote! {
                const { assert!(::core::mem::size_of::<#ret_ty>() <= 8) };
                let ret: #ret_ty = ::core::ptr::read_unaligned(ret.as_ptr() as *const #ret_ty);
            },
            quote!(ret,),
        ),
        None => (quote!(let _ = ret;), quote!()),
    };
    let decode_call = match &ret_ty {
        Some(ret_ty) => quote! {
            let value: #ret_ty = #decode_reply_fn(decoder, #(#reads),*);
            let mut bytes: ReturnBytes = [0; 8];
            ::core::ptr::write_unaligned(bytes.as_mut_ptr() as *mut #ret_ty, value);
            bytes
        },
        None => quote! {
            #decode_reply_fn(decoder, #(#reads),*);
            [0; 8]
        },
    };

    let wrappers = quote! {
        unsafe fn #w_sizeof(
            protocol: &RendererProtocol,
            args: &[*mut ::core::ffi::c_void],
        ) -> Result<usize, EncodeError> {
            unsafe { #sizeof_fn(protocol, #(#reads),*) }
        }
        unsafe fn #w_encode(
            encoder: &mut Encoder<'_>,
            flags: u32,
            args: &[*mut ::core::ffi::c_void],
        ) {
            unsafe { #encode_fn(encoder, flags, #(#reads),*) }
        }
        unsafe fn #w_sizeof_reply(
            protocol: &RendererProtocol,
            args: &[*mut ::core::ffi::c_void],
        ) -> Result<usize, EncodeError> {
            unsafe { #sizeof_reply_fn(protocol, #(#reads),*) }
        }
        unsafe fn #w_encode_reply(
            encoder: &mut Encoder<'_>,
            ret: ReturnBytes,
            args: &[*mut ::core::ffi::c_void],
        ) {
            unsafe {
                #ret_read
                #encode_reply_fn(encoder, #ret_arg #(#reads),*)
            }
        }
        unsafe fn #w_decode_reply(
            decoder: &mut Decoder<'_>,
            args: &[*mut ::core::ffi::c_void],
        ) -> ReturnBytes {
            unsafe { #decode_call }
        }
    };

    let entry = quote! {
        CommandEntry {
            name: #name,
            command_type: #command_const.0 as u32,
            ret_size: #ret_size,
            sizeof: #w_sizeof,
            encode: #w_encode,
            sizeof_reply: #w_sizeof_reply,
            encode_reply: #w_encode_reply,
            decode_reply: #w_decode_reply,
        }
    };
    Ok((wrappers, entry))
}
