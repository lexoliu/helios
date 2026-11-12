//! The generated types: scalars, enums, handles, structs and unions, the
//! extension table, and the layout assertions.

use std::collections::BTreeSet;

use proc_macro2::{Literal, TokenStream};
use quote::quote;

use super::names::{base_type, decl_type, ident};
use crate::analysis::Analysis;
use crate::layout::Layouts;
use crate::policy;
use crate::registry::{Category, RegistryError, TypeId, parse_int};

type Result<T> = std::result::Result<T, RegistryError>;

/// Which types get a full definition and which are opaque.
pub struct TypeSet {
    /// Structs and unions defined with their members, in registry order.
    pub full: Vec<TypeId>,
    /// Types only ever reached through a pointer, and not serializable.
    pub opaque: Vec<TypeId>,
}

pub fn type_set(an: &Analysis) -> Result<TypeSet> {
    let mut full: BTreeSet<TypeId> = an.generated_structs.iter().copied().collect();
    let mut opaque: BTreeSet<TypeId> = BTreeSet::new();
    let mut work: Vec<TypeId> = full.iter().copied().collect();
    let commands = an.generated_commands.clone();
    // Types referenced by value from a defined type must be defined too.
    while let Some(id) = work.pop() {
        for var in &an.reg.types[id].variables {
            let base = var.ty.base;
            if !matches!(
                an.reg.types[base].category(),
                Category::Struct | Category::Union
            ) {
                continue;
            }
            if var.ty.is_pointer() {
                continue;
            }
            if full.insert(base) {
                work.push(base);
            }
        }
    }
    let mut consider = |base: TypeId, full: &BTreeSet<TypeId>| {
        let category = an.reg.types[base].category();
        let name = an.reg.types[base].name.as_str();
        let is_opaque = match category {
            Category::Struct | Category::Union => !full.contains(&base),
            Category::Default => !matches!(
                name,
                "void"
                    | "char"
                    | "size_t"
                    | "float"
                    | "double"
                    | "uint8_t"
                    | "uint16_t"
                    | "uint32_t"
                    | "uint64_t"
                    | "int8_t"
                    | "int16_t"
                    | "int32_t"
                    | "int64_t"
            ),
            _ => false,
        };
        if is_opaque {
            opaque.insert(base);
        }
    };
    for &id in full.iter().chain(commands.iter()) {
        let ty = &an.reg.types[id];
        for var in &ty.variables {
            consider(var.ty.base, &full);
        }
        if let Some(ret) = &ty.ret {
            consider(ret.base, &full);
        }
    }
    // `VkBaseInStructure` and `VkBaseOutStructure` are the runtime's.
    let skip = ["VkBaseInStructure", "VkBaseOutStructure"];
    let drop_runtime = |id: &TypeId| !skip.contains(&an.reg.types[*id].name.as_str());
    Ok(TypeSet {
        full: full.into_iter().filter(drop_runtime).collect(),
        opaque: opaque.into_iter().filter(drop_runtime).collect(),
    })
}

fn int_literal(value: i128, bits: u32) -> Result<TokenStream> {
    // Enumerators of a 32-bit enum may be spelled as unsigned hex.
    let wrapped: i128 = match bits {
        32 => i128::from(value as u32 as i32),
        64 => i128::from(value as u64 as i64),
        _ => return Err(RegistryError::unsupported(format!("{bits}-bit enum"))),
    };
    let magnitude = wrapped.unsigned_abs();
    let literal = Literal::u128_unsuffixed(magnitude);
    Ok(if wrapped < 0 {
        quote!(-#literal)
    } else {
        quote!(#literal)
    })
}

pub fn scalar_items(an: &Analysis) -> Result<TokenStream> {
    let mut out = TokenStream::new();
    // Basetypes (`VkBool32`, `VkDeviceSize`), by their typedef.
    for &id in an
        .supported
        .get(&Category::Basetype)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let ty = &an.reg.types[id];
        let Some(typedef) = ty.typedef else { continue };
        if !an.is_serializable_type(typedef) && an.reg.types[typedef].name != "void" {
            continue;
        }
        let name = ident(&ty.name);
        let target = base_type(an, typedef)?;
        out.extend(quote!(pub type #name = #target;));
    }
    for &id in an
        .supported
        .get(&Category::Bitmask)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let ty = &an.reg.types[id];
        let name = ident(&ty.name);
        let target = base_type(an, ty.typedef.expect("a bitmask is a typedef"))?;
        out.extend(quote!(pub type #name = #target;));
    }
    for &id in an
        .supported
        .get(&Category::Funcpointer)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let name = ident(&an.reg.types[id].name);
        out.extend(quote!(pub type #name = Option<unsafe extern "C" fn()>;));
    }
    for &id in an
        .supported
        .get(&Category::Enum)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let ty = &an.reg.types[id];
        let enums = ty.enums.as_ref().expect("an enum has values");
        if enums.values.is_empty() {
            continue;
        }
        let name = ident(&ty.name);
        let (repr, bits) = match enums.bitwidth {
            32 => (quote!(i32), 32),
            64 => (quote!(u64), 64),
            other => {
                return Err(RegistryError::unsupported(format!(
                    "enum {} has bitwidth {other}",
                    ty.name
                )));
            }
        };
        out.extend(quote! {
            #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
            #[repr(transparent)]
            pub struct #name(pub #repr);
            scalar_newtype!(#name, #repr);
        });
        let mut seen = BTreeSet::new();
        for (key, value) in &enums.values {
            if !seen.insert(key.as_str()) {
                continue;
            }
            let value = parse_int(value)?;
            let literal = if bits == 64 {
                let literal = Literal::u64_unsuffixed(u64::try_from(value).map_err(|_| {
                    RegistryError::unsupported(format!("{key} does not fit a 64-bit flag"))
                })?);
                quote!(#literal)
            } else {
                int_literal(value, bits)?
            };
            let constant = ident(key);
            out.extend(quote!(pub const #constant: #name = #name(#literal);));
        }
    }
    Ok(out)
}

pub fn handle_items(an: &Analysis) -> TokenStream {
    let mut out = TokenStream::new();
    for &id in an
        .supported
        .get(&Category::Handle)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let ty = &an.reg.types[id];
        let name = ident(&ty.name);
        out.extend(if ty.dispatchable {
            quote! {
                #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
                #[repr(transparent)]
                pub struct #name(pub *mut ObjectBase);
            }
        } else {
            quote! {
                #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
                #[repr(transparent)]
                pub struct #name(pub u64);
            }
        });
    }
    out
}

pub fn aggregate_items(an: &Analysis, set: &TypeSet) -> Result<TokenStream> {
    let mut out = TokenStream::new();
    for &id in &set.opaque {
        let name = ident(&an.reg.types[id].name);
        out.extend(quote! {
            #[repr(C)]
            pub struct #name {
                _opaque: [u8; 0],
            }
        });
    }
    for &id in &set.full {
        let ty = &an.reg.types[id];
        let name = ident(&ty.name);
        let mut fields = TokenStream::new();
        // The fields follow the C declaration; the wire order does not.
        for (_, var) in ty.declared() {
            let field = ident(&var.name);
            let field_ty = decl_type(an, &var.ty, &var.name)?;
            fields.extend(quote!(pub #field: #field_ty,));
        }
        out.extend(if ty.category() == Category::Union {
            quote! {
                #[derive(Clone, Copy)]
                #[repr(C)]
                pub union #name { #fields }
            }
        } else {
            quote! {
                #[derive(Clone, Copy)]
                #[repr(C)]
                pub struct #name { #fields }
            }
        });
    }
    Ok(out)
}

/// `size_of`, `align_of` and `offset_of` assertions for both pointer widths.
pub fn layout_assertions(an: &Analysis, set: &TypeSet) -> Result<TokenStream> {
    let mut out = TokenStream::new();
    for (width, cfg) in [(8u64, "64"), (4u64, "32")] {
        let layouts = Layouts::new(&an.reg, width);
        let mut checks = TokenStream::new();
        for &id in &set.full {
            let ty = &an.reg.types[id];
            let name = ident(&ty.name);
            let layout = layouts.of_aggregate(id)?;
            let (size, align) = (
                Literal::u64_unsuffixed(layout.layout.size),
                Literal::u64_unsuffixed(layout.layout.align),
            );
            checks.extend(quote! {
                assert!(::core::mem::size_of::<#name>() == #size);
                assert!(::core::mem::align_of::<#name>() == #align);
            });
            if ty.category() == Category::Struct {
                for (var, offset) in ty.variables.iter().zip(&layout.offsets) {
                    let field = ident(&var.name);
                    let offset = Literal::u64_unsuffixed(*offset);
                    checks.extend(quote! {
                        assert!(::core::mem::offset_of!(#name, #field) == #offset);
                    });
                }
            }
        }
        let cfg = Literal::string(cfg);
        out.extend(quote! {
            #[cfg(target_pointer_width = #cfg)]
            const _: () = { #checks };
        });
    }
    Ok(out)
}

pub fn info_items(an: &Analysis) -> Result<TokenStream> {
    let mut extensions: Vec<_> = an
        .reg
        .extensions
        .iter()
        .filter(|ext| policy::EXTENSION_LIST.contains(&ext.name.as_str()))
        .collect();
    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    let count = Literal::usize_unsuffixed(extensions.len());
    let max = extensions
        .iter()
        .map(|ext| ext.number)
        .max()
        .ok_or_else(|| RegistryError::inconsistent("no extension is supported"))?;
    let entries = extensions.iter().map(|ext| {
        let (name, number, version) = (&ext.name, ext.number, ext.version);
        quote!(ExtensionInfo { name: #name, number: #number, spec_version: #version })
    });
    let [variant, major, minor, patch] = an.reg.vk_xml_version;
    let wire = policy::WIRE_FORMAT_VERSION;
    Ok(quote! {
        pub const WIRE_FORMAT_VERSION: u32 = #wire;
        pub const VK_XML_VERSION: u32 = make_api_version(#variant, #major, #minor, #patch);
        pub const MAX_EXTENSION_NUMBER: u32 = #max;
        /// Sorted by name, for binary search.
        pub const EXTENSIONS: [ExtensionInfo; #count] = [#(#entries),*];
    })
}
