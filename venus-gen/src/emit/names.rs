//! Rust spellings of registry names and types.

use proc_macro2::{Ident, Span, TokenStream};
use quote::quote;

use crate::analysis::Analysis;
use crate::registry::{Category, RegistryError, TypeId, TypeRef};

type Result<T> = std::result::Result<T, RegistryError>;

const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro", "override",
    "priv", "typeof", "unsized", "virtual", "yield", "try", "gen",
];

/// An identifier for a registry name; Rust keywords become raw identifiers.
pub fn ident(name: &str) -> Ident {
    if KEYWORDS.contains(&name) {
        Ident::new_raw(name, Span::call_site())
    } else {
        Ident::new(name, Span::call_site())
    }
}

/// `vn_<what>_<name>`.
pub fn func(what: &str, name: &str) -> Ident {
    ident(&format!("vn_{what}_{name}"))
}

/// One level of C indirection around a base type.
#[derive(Clone, Debug)]
pub enum Level {
    Ptr { konst: bool },
    Array { len: u64 },
}

/// A declared type split into its base type and its levels, outermost first
/// (a static array encloses the pointers of its elements).
#[derive(Clone, Debug)]
pub struct Shape {
    pub base: TypeId,
    pub levels: Vec<Level>,
}

impl Shape {
    pub fn of(an: &Analysis, ty: &TypeRef, what: &str) -> Result<Self> {
        let mut levels = Vec::new();
        if let Some(dim) = &ty.decor.dim {
            let len = crate::layout::Layouts::new(&an.reg, 8).array_len(dim, what)?;
            levels.push(Level::Array { len });
        }
        // `ref_quals[k]` qualifies what the k-th `*` points to; the outermost
        // pointer is the last one.
        for qual in ty.decor.ref_quals.iter().rev() {
            levels.push(Level::Ptr {
                konst: qual.contains("const"),
            });
        }
        Ok(Self {
            base: ty.base,
            levels,
        })
    }
}

/// The Rust type of the base type of a registry type.
pub fn base_type(an: &Analysis, id: TypeId) -> Result<TokenStream> {
    let ty = &an.reg.types[id];
    Ok(match ty.category() {
        Category::Default => match ty.name.as_str() {
            "float" => quote!(f32),
            "double" => quote!(f64),
            "uint8_t" => quote!(u8),
            "uint16_t" => quote!(u16),
            "uint32_t" => quote!(u32),
            "uint64_t" => quote!(u64),
            "int8_t" => quote!(i8),
            "int16_t" => quote!(i16),
            "int32_t" => quote!(i32),
            "int64_t" => quote!(i64),
            "char" => quote!(::core::ffi::c_char),
            "size_t" => quote!(usize),
            "void" => quote!(::core::ffi::c_void),
            other => {
                return Err(RegistryError::unsupported(format!(
                    "C type `{other}` has no Rust type"
                )));
            }
        },
        _ => {
            let name = ident(&ty.name);
            quote!(#name)
        }
    })
}

/// The Rust type of a shape: `*const T`, `[T; N]`, `*mut *const T`.
pub fn shape_type(an: &Analysis, shape: &Shape) -> Result<TokenStream> {
    let mut tokens = base_type(an, shape.base)?;
    for level in shape.levels.iter().rev() {
        tokens = match level {
            Level::Ptr { konst: true } => quote!(*const #tokens),
            Level::Ptr { konst: false } => quote!(*mut #tokens),
            Level::Array { len } => {
                let len = proc_macro2::Literal::u64_unsuffixed(*len);
                quote!([#tokens; #len])
            }
        };
    }
    Ok(tokens)
}

/// The Rust type of a declared type.
pub fn decl_type(an: &Analysis, ty: &TypeRef, what: &str) -> Result<TokenStream> {
    shape_type(an, &Shape::of(an, ty, what)?)
}
