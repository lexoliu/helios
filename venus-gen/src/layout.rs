//! C struct layout of the registry's types for a given pointer width, as a C
//! compiler lays them out (natural alignment, no packing, no bit-fields).
//!
//! The generated Rust types are `#[repr(C)]`; the layout assertions the
//! generator emits come from here, so a layout difference between the Rust
//! types and the C ABI of a 32-bit pointer target fails the build instead of
//! corrupting the wire.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::policy;
use crate::registry::{Category, Registry, RegistryError, TypeDef, TypeId, TypeRef, Variable};

type Result<T> = std::result::Result<T, RegistryError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: u64,
    pub align: u64,
}

impl Layout {
    fn round_up(value: u64, align: u64) -> u64 {
        value.div_ceil(align) * align
    }
}

/// A struct's layout and the byte offset of each member, in declaration order.
#[derive(Clone, Debug)]
pub struct StructLayout {
    pub layout: Layout,
    pub offsets: Vec<u64>,
}

pub struct Layouts<'a> {
    reg: &'a Registry,
    pointer_width: u64,
    memo: RefCell<HashMap<TypeId, StructLayout>>,
}

impl<'a> Layouts<'a> {
    pub fn new(reg: &'a Registry, pointer_width: u64) -> Self {
        Self {
            reg,
            pointer_width,
            memo: RefCell::new(HashMap::new()),
        }
    }

    pub fn pointer_width(&self) -> u64 {
        self.pointer_width
    }

    fn scalar(&self, size: u64) -> Layout {
        Layout { size, align: size }
    }

    /// The layout of a type used by value.
    pub fn of_type(&self, id: TypeId) -> Result<Layout> {
        let ty = &self.reg.types[id];
        match ty.category() {
            Category::Default => {
                let name = ty.name.as_str();
                if let Some((_, size)) = policy::PRIMITIVE_TYPES.iter().find(|(n, _)| *n == name) {
                    Ok(self.scalar(u64::from(*size)))
                } else if name == "char" {
                    Ok(self.scalar(1))
                } else if name == "size_t" {
                    Ok(self.scalar(self.pointer_width))
                } else {
                    Err(RegistryError::unsupported(format!(
                        "type {name} has no layout"
                    )))
                }
            }
            Category::Basetype | Category::Bitmask => match ty.typedef {
                Some(typedef) => self.of_type(typedef),
                None => Err(RegistryError::unsupported(format!(
                    "type {} has no layout",
                    ty.name
                ))),
            },
            Category::Enum => {
                Ok(self.scalar(u64::from(ty.enums.as_ref().map_or(32, |e| e.bitwidth) / 8)))
            }
            Category::Handle => Ok(self.scalar(if ty.dispatchable {
                self.pointer_width
            } else {
                8
            })),
            Category::Funcpointer => Ok(self.scalar(self.pointer_width)),
            Category::Struct | Category::Union => Ok(self.of_aggregate(id)?.layout),
            Category::Include | Category::Define | Category::Command => Err(
                RegistryError::unsupported(format!("type {} has no layout", ty.name)),
            ),
        }
    }

    /// The layout of a variable, declaration decorations included.
    pub fn of_variable(&self, var: &Variable) -> Result<Layout> {
        self.of_ref(&var.ty, &var.name)
    }

    pub fn of_ref(&self, ty: &TypeRef, what: &str) -> Result<Layout> {
        if ty.decor.bit_size.is_some() {
            return Err(RegistryError::unsupported(format!("{what} is a bit-field")));
        }
        let element = if ty.is_pointer() {
            self.scalar(self.pointer_width)
        } else {
            self.of_type(ty.base)?
        };
        match &ty.decor.dim {
            None => Ok(element),
            Some(dim) => Ok(Layout {
                size: element.size * self.array_len(dim, what)?,
                align: element.align,
            }),
        }
    }

    pub fn array_len(&self, dim: &str, what: &str) -> Result<u64> {
        if dim.contains(']') || dim.contains('[') {
            return Err(RegistryError::unsupported(format!(
                "{what} is a multi-dimensional array"
            )));
        }
        let value = self.reg.constant(dim)?;
        u64::try_from(value)
            .map_err(|_| RegistryError::unsupported(format!("{what} has array size {dim}")))
    }

    /// The layout of a struct or union and its member offsets.
    pub fn of_aggregate(&self, id: TypeId) -> Result<StructLayout> {
        if let Some(known) = self.memo.borrow().get(&id) {
            return Ok(known.clone());
        }
        let ty: &TypeDef = &self.reg.types[id];
        let is_union = ty.category() == Category::Union;
        let mut offset = 0;
        let mut align = 1;
        // Indexed like `variables`; the C declaration order places them.
        let mut offsets = vec![0; ty.variables.len()];
        let mut union_size = 0;
        for (index, var) in ty.declared() {
            let member = self.of_variable(var)?;
            align = align.max(member.align);
            if is_union {
                union_size = union_size.max(member.size);
            } else {
                offset = Layout::round_up(offset, member.align);
                offsets[index] = offset;
                offset += member.size;
            }
        }
        let size = Layout::round_up(if is_union { union_size } else { offset }, align);
        let result = StructLayout {
            layout: Layout { size, align },
            offsets,
        };
        self.memo.borrow_mut().insert(id, result.clone());
        Ok(result)
    }
}
