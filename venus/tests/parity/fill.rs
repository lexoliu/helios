//! Builds the arguments of a command as C memory of the host's layout, from
//! the registry alone: every pointer that the wire carries points to storage
//! the filler owns, every length member holds [`COUNT`], and every other
//! scalar holds a value derived from a seed.
//!
//! The same memory is handed to Mesa's C encoders and to the generated Rust
//! encoders, so what is compared is the encoders, not the filler: nothing
//! here needs to know the wire format, only the registry's declarations.

use core::ptr::null_mut;

use helios_venus_gen::analysis::{Analysis, is_blob};
use helios_venus_gen::layout::Layouts;
use helios_venus_gen::policy;
use helios_venus_gen::registry::{Category, TypeId, TypeRef, Variable};

/// The value of every length member.
pub const COUNT: u64 = 2;
/// Elements allocated for an array; lengths such as `2 * count` still fit.
const CAPACITY: usize = 8;
/// Elements of an array that are filled; the rest stay zero.
const FILLED: usize = 2;
/// Pointers followed before the filler stops and leaves a pointer null.
const MAX_DEPTH: usize = 5;
/// A structure type no pNext dispatch knows.
const UNKNOWN_STRUCTURE_TYPE: u32 = 0x7FFF_FFF0;
const POINTER_SIZE: usize = 8;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct FillError(pub String);

type Result<T> = std::result::Result<T, FillError>;

fn error<T>(message: String) -> Result<T> {
    Err(FillError(message))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PNext {
    /// Every pNext pointer is null.
    Null,
    /// A pNext pointer of a struct near the root carries every serializable
    /// struct that may extend it; deeper ones carry two.
    Chain,
    /// As `Chain`, with a structure of an unknown type at the head.
    ChainWithUnknown,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub pnext: PNext,
    /// Chooses enumerants and flags, which decide which members the wire
    /// carries (a sharing mode, framebuffer flags).
    pub layout_seed: u64,
    /// Chooses plain integers, floats and handle ids.
    pub value_seed: u64,
}

/// Zeroed storage that never moves once handed out.
#[derive(Default)]
struct Arena {
    blocks: Vec<Box<[u128]>>,
}

impl Arena {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        let mut block = vec![0u128; size.div_ceil(16).max(1)].into_boxed_slice();
        let pointer = block.as_mut_ptr().cast::<u8>();
        self.blocks.push(block);
        pointer
    }
}

fn mix(mut x: u64) -> u64 {
    // splitmix64: a well-known 64-bit mixer, so seeds that differ by one give
    // unrelated values.
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The storage of each variable of a struct or of a command's parameters.
struct Container<'a> {
    vars: &'a [Variable],
    storage: Vec<*mut u8>,
}

pub struct Filler<'a> {
    an: &'a Analysis,
    layouts: Layouts<'a>,
    arena: Arena,
    cfg: Config,
    value_counter: u64,
    layout_counter: u64,
    structure_type: TypeId,
}

impl<'a> Filler<'a> {
    pub fn new(an: &'a Analysis, cfg: Config) -> Result<Self> {
        let structure_type = an
            .reg
            .lookup("VkStructureType")
            .ok_or_else(|| FillError("the registry has no VkStructureType".into()))?;
        Ok(Self {
            an,
            layouts: Layouts::new(&an.reg, POINTER_SIZE as u64),
            arena: Arena::default(),
            cfg,
            value_counter: 0,
            layout_counter: 0,
            structure_type,
        })
    }

    /// The storage of each parameter of `command`, filled.
    pub fn command_args(&mut self, command: TypeId) -> Result<Vec<*mut u8>> {
        let an = self.an;
        let ty = &an.reg.types[command];
        let mut storage = Vec::new();
        for var in &ty.variables {
            let layout = self
                .layouts
                .of_variable(var)
                .map_err(|e| FillError(format!("{}.{}: {e}", ty.name, var.name)))?;
            storage.push(self.arena.alloc(layout.size as usize));
        }
        let container = Container {
            vars: &ty.variables,
            storage,
        };
        let done = vec![false; ty.variables.len()];
        self.fill_container(&container, &done, 0, &ty.name)?;
        Ok(container.storage)
    }

    fn next_value(&mut self) -> u64 {
        self.value_counter += 1;
        mix(self
            .cfg
            .value_seed
            .wrapping_mul(0x1_0000_0001)
            .wrapping_add(self.value_counter))
    }

    fn next_layout(&mut self) -> u64 {
        self.layout_counter += 1;
        mix(self
            .cfg
            .layout_seed
            .wrapping_mul(0x2_0000_0003)
            .wrapping_add(self.layout_counter)
            ^ 0xA5A5)
    }

    /// Fills the members of a struct or the parameters of a command that
    /// `done` does not mark, then sets the lengths and selectors they name.
    fn fill_container(
        &mut self,
        container: &Container<'_>,
        done: &[bool],
        depth: usize,
        owner: &str,
    ) -> Result<()> {
        let mut selectors: Vec<(String, i128)> = Vec::new();
        for (index, var) in container.vars.iter().enumerate() {
            if done[index] {
                continue;
            }
            let at = container.storage[index];
            if let Some(selector) = &var.attrs.selector {
                selectors.push((
                    selector.clone(),
                    self.fill_selected_union(var, at, depth, owner)?,
                ));
            } else {
                self.fill_var(var, at, depth, owner)?;
            }
        }
        for (name, value) in selectors {
            let index = container
                .vars
                .iter()
                .position(|v| v.name == name)
                .ok_or_else(|| FillError(format!("{owner}: selector {name} is not a member")))?;
            let size = self.size_of_base(container.vars[index].ty.base, &name)?;
            // SAFETY: the storage of a member has its layout's size.
            unsafe { write_int(container.storage[index], size, value as u64) };
        }
        self.apply_lengths(container, owner)
    }

    /// Fills a union chosen by a sibling selector and returns the selector's
    /// value.
    fn fill_selected_union(
        &mut self,
        var: &Variable,
        at: *mut u8,
        depth: usize,
        owner: &str,
    ) -> Result<i128> {
        let an = self.an;
        let union = &an.reg.types[var.ty.base];
        let selector_type = union
            .selector_type
            .as_ref()
            .ok_or_else(|| {
                FillError(format!(
                    "{owner}.{}: its union has no selector type",
                    var.name
                ))
            })?
            .base;
        let candidates: Vec<usize> = union
            .variables
            .iter()
            .enumerate()
            .filter(|(_, member)| !member.attrs.selection.is_empty())
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() {
            return error(format!(
                "{owner}.{}: no union member names a selector value",
                var.name
            ));
        }
        let pick = candidates[(self.next_layout() % candidates.len() as u64) as usize];
        let member = &union.variables[pick];
        let value = self
            .an
            .reg
            .enum_value(selector_type, &member.attrs.selection[0])
            .map_err(|e| FillError(format!("{owner}.{}: {e}", var.name)))?;
        let member = member.clone();
        self.fill_var(&member, at, depth + 1, owner)?;
        Ok(value)
    }

    fn fill_var(&mut self, var: &Variable, at: *mut u8, depth: usize, owner: &str) -> Result<()> {
        let ty = &var.ty;
        if ty.decor.bit_size.is_some() {
            return error(format!("{owner}.{} is a bit-field", var.name));
        }
        let Some(dim) = &ty.decor.dim else {
            return self.fill_element(var, ty, at, depth, owner);
        };
        let count =
            self.layouts
                .array_len(dim, &var.name)
                .map_err(|e| FillError(format!("{owner}.{}: {e}", var.name)))? as usize;
        let element = if ty.is_pointer() {
            POINTER_SIZE
        } else {
            self.layouts
                .of_type(ty.base)
                .map_err(|e| FillError(format!("{owner}.{}: {e}", var.name)))?
                .size as usize
        };
        if !ty.is_pointer() && self.an.reg.types[ty.base].name == "char" {
            // SAFETY: a character array has `count >= 3` bytes.
            unsafe { std::ptr::copy_nonoverlapping(b"ab".as_ptr(), at, 2) };
            return Ok(());
        }
        for i in 0..count {
            // SAFETY: `i < count` elements of `element` bytes are the array.
            let at = unsafe { at.add(i * element) };
            self.fill_element(var, ty, at, depth, owner)?;
        }
        Ok(())
    }

    fn fill_element(
        &mut self,
        var: &Variable,
        ty: &TypeRef,
        at: *mut u8,
        depth: usize,
        owner: &str,
    ) -> Result<()> {
        if ty.is_pointer() {
            self.fill_pointer(var, ty, at, depth, owner)
        } else {
            self.fill_base(var, ty.base, at, depth, owner)
        }
    }

    fn fill_pointer(
        &mut self,
        var: &Variable,
        ty: &TypeRef,
        at: *mut u8,
        depth: usize,
        owner: &str,
    ) -> Result<()> {
        // What the wire cannot carry stays null.
        if !self.an.is_serializable(var) || depth >= MAX_DEPTH {
            return Ok(());
        }
        let c_string = var.has_c_string();
        let pointer = match ty.indirection_depth() {
            1 if c_string => self.string(),
            1 if is_blob(&self.an.reg, var) => {
                let block = self.arena.alloc(CAPACITY * 8);
                for i in 0..CAPACITY * 8 {
                    // SAFETY: inside the block just allocated.
                    unsafe { *block.add(i) = (self.next_value() & 0xFF) as u8 };
                }
                block
            }
            1 => self.elements(var, ty.base, depth, owner)?,
            2 => {
                let count = if var.has_len() { CAPACITY } else { 1 };
                let outer = self.arena.alloc(count * POINTER_SIZE);
                for i in 0..FILLED.min(count) {
                    let inner = if c_string {
                        self.string()
                    } else {
                        self.elements(var, ty.base, depth, owner)?
                    };
                    // SAFETY: `i < count` pointer slots were allocated.
                    unsafe { *outer.cast::<*mut u8>().add(i) = inner };
                }
                outer
            }
            levels => {
                return error(format!(
                    "{owner}.{} has {levels} levels of pointers",
                    var.name
                ));
            }
        };
        // SAFETY: `at` is the storage of one pointer.
        unsafe { *at.cast::<*mut u8>() = pointer };
        Ok(())
    }

    fn string(&mut self) -> *mut u8 {
        let block = self.arena.alloc(16);
        // SAFETY: the block has 16 bytes.
        unsafe { std::ptr::copy_nonoverlapping(c"ab".as_ptr().cast::<u8>(), block, 3) };
        block
    }

    /// A block of elements of `base`, the first [`FILLED`] of them filled.
    fn elements(
        &mut self,
        var: &Variable,
        base: TypeId,
        depth: usize,
        owner: &str,
    ) -> Result<*mut u8> {
        let size = self
            .layouts
            .of_type(base)
            .map_err(|e| FillError(format!("{owner}.{}: {e}", var.name)))?
            .size as usize;
        let count = if var.has_len() { CAPACITY } else { 1 };
        let block = self.arena.alloc(size * count);
        for i in 0..FILLED.min(count) {
            // SAFETY: `i < count` elements of `size` bytes were allocated.
            let at = unsafe { block.add(i * size) };
            self.fill_base(var, base, at, depth + 1, owner)?;
        }
        Ok(block)
    }

    fn fill_base(
        &mut self,
        var: &Variable,
        base: TypeId,
        at: *mut u8,
        depth: usize,
        owner: &str,
    ) -> Result<()> {
        let an = self.an;
        let ty = &an.reg.types[base];
        match ty.category() {
            Category::Default => {
                let value = self.next_value();
                match ty.name.as_str() {
                    // Finite values: a NaN would compare unequal as floats
                    // and says nothing about the encoder.
                    "float" => unsafe {
                        at.cast::<f32>()
                            .write_unaligned((value % 1000) as f32 / 8.0)
                    },
                    "double" => unsafe {
                        at.cast::<f64>()
                            .write_unaligned((value % 1000) as f64 / 8.0)
                    },
                    _ => {
                        let size = self.size_of_base(base, &var.name)?;
                        // SAFETY: `at` holds a value of this type.
                        unsafe { write_int(at, size, value) };
                    }
                }
                Ok(())
            }
            Category::Basetype => match ty.typedef {
                Some(typedef) => self.fill_base(var, typedef, at, depth, owner),
                None => error(format!("{owner}.{}: {} has no typedef", var.name, ty.name)),
            },
            Category::Bitmask => {
                let size = self.size_of_base(base, &var.name)?;
                let value = self.next_layout();
                // SAFETY: `at` holds a flags value.
                unsafe { write_int(at, size, value) };
                Ok(())
            }
            Category::Enum => {
                let size = self.size_of_base(base, &var.name)?;
                let values: Vec<i128> = ty
                    .enums
                    .iter()
                    .flat_map(|e| e.values.iter())
                    .filter_map(|(_, text)| helios_venus_gen::registry::parse_int(text).ok())
                    .collect();
                let pick = self.next_layout();
                let value = if values.is_empty() {
                    0
                } else {
                    values[(pick % values.len() as u64) as usize]
                };
                // SAFETY: `at` holds an enumeration value.
                unsafe { write_int(at, size, value as u64) };
                Ok(())
            }
            Category::Handle => {
                let id = 0x1000 + (self.next_value() & 0xFFFF_FFFF);
                if ty.dispatchable {
                    let object = self.arena.alloc(16);
                    // SAFETY: the object has 16 bytes, its id at the start.
                    unsafe {
                        object.cast::<u64>().write(id);
                        at.cast::<*mut u8>().write(object);
                    }
                } else {
                    // SAFETY: a non-dispatchable handle is 8 bytes.
                    unsafe { at.cast::<u64>().write_unaligned(id) };
                }
                Ok(())
            }
            Category::Struct => self.fill_struct(base, at, depth, None),
            Category::Union => {
                let tag = policy::UNION_DEFAULT_TAGS
                    .iter()
                    .find(|(name, _)| *name == ty.name)
                    .map(|(_, tag)| *tag as usize)
                    .ok_or_else(|| {
                        FillError(format!(
                            "{owner}.{}: union {} is neither selected nor has a default tag",
                            var.name, ty.name
                        ))
                    })?;
                let member = ty.variables[tag].clone();
                self.fill_var(&member, at, depth + 1, &ty.name.clone())
            }
            Category::Funcpointer => Ok(()),
            category => error(format!(
                "{owner}.{}: {category:?} cannot be filled",
                var.name
            )),
        }
    }

    /// Fills a struct. `pnext` replaces the filling of its pNext member (the
    /// next link of a chain being built).
    fn fill_struct(
        &mut self,
        id: TypeId,
        at: *mut u8,
        depth: usize,
        pnext: Option<*mut u8>,
    ) -> Result<()> {
        let an = self.an;
        let ty = &an.reg.types[id];
        let name = ty.name.as_str();
        let layout = self
            .layouts
            .of_aggregate(id)
            .map_err(|e| FillError(format!("{name}: {e}")))?;
        let mut storage = Vec::new();
        let mut done = Vec::new();
        for (var, offset) in ty.variables.iter().zip(&layout.offsets) {
            // SAFETY: the offsets are inside the struct's layout.
            let member = unsafe { at.add(*offset as usize) };
            storage.push(member);
            done.push(matches!(
                (&ty.s_type, var.name.as_str()),
                (Some(_), "sType") | (_, "pNext")
            ));
            match (&ty.s_type, var.name.as_str()) {
                (Some(s_type), "sType") => {
                    let value = an
                        .reg
                        .enum_value(self.structure_type, s_type)
                        .map_err(|e| FillError(format!("{name}: {e}")))?;
                    // SAFETY: sType is 4 bytes.
                    unsafe { member.cast::<u32>().write_unaligned(value as u32) };
                }
                (_, "pNext") => {
                    let head = match pnext {
                        Some(next) => next,
                        None => self.chain(id, depth)?,
                    };
                    // SAFETY: pNext is a pointer.
                    unsafe { member.cast::<*mut u8>().write_unaligned(head) };
                }
                _ => {}
            }
        }
        let container = Container {
            vars: &ty.variables,
            storage,
        };
        self.fill_container(&container, &done, depth, name)
    }

    /// The head of a pNext chain for a struct, or null.
    fn chain(&mut self, owner: TypeId, depth: usize) -> Result<*mut u8> {
        if self.cfg.pnext == PNext::Null {
            return Ok(null_mut());
        }
        let (mut members, _skipped) = self.an.chain(owner);
        if members.is_empty() {
            return Ok(null_mut());
        }
        if depth > 1 {
            let start = (self.next_layout() % members.len() as u64) as usize;
            members.rotate_left(start);
            members.truncate(2);
        }
        let mut nodes = Vec::new();
        for &member in &members {
            let size = self
                .layouts
                .of_type(member)
                .map_err(|e| FillError(format!("{}: {e}", self.an.reg.types[member].name)))?
                .size as usize;
            nodes.push((member, self.arena.alloc(size)));
        }
        let mut next = null_mut();
        for &(member, node) in nodes.iter().rev() {
            self.fill_struct(member, node, depth + 1, Some(next))?;
            next = node;
        }
        if self.cfg.pnext == PNext::ChainWithUnknown {
            let unknown = self.arena.alloc(16);
            // SAFETY: the block has 16 bytes: sType, padding, pNext.
            unsafe {
                unknown.cast::<u32>().write(UNKNOWN_STRUCTURE_TYPE);
                unknown.add(8).cast::<*mut u8>().write(next);
            }
            next = unknown;
        }
        Ok(next)
    }

    fn size_of_base(&self, base: TypeId, what: &str) -> Result<usize> {
        self.layouts
            .of_type(base)
            .map(|l| l.size as usize)
            .map_err(|e| FillError(format!("{what}: {e}")))
    }

    /// Sets every member that another variable's length names to [`COUNT`],
    /// and every member that gives the stride of an array to the size of its
    /// elements.
    fn apply_lengths(&mut self, container: &Container<'_>, owner: &str) -> Result<()> {
        for var in container.vars {
            if let Some(stride) = &var.attrs.stride {
                let element = self
                    .layouts
                    .of_type(var.ty.base)
                    .map_err(|e| FillError(format!("{owner}.{}: {e}", var.name)))?
                    .size;
                for (at, size) in self.resolve(container, stride, owner)? {
                    // SAFETY: `resolve` returns the storage of scalars of `size` bytes.
                    unsafe { write_int(at, size, element) };
                }
            }
            for len in &var.attrs.lens {
                if len.name.is_empty() {
                    continue;
                }
                for (at, size) in self.resolve(container, &len.name, owner)? {
                    // SAFETY: `resolve` returns the storage of scalars of `size` bytes.
                    unsafe { write_int(at, size, COUNT) };
                }
            }
        }
        Ok(())
    }

    /// The storage and size of each scalar a length path names. A pointer on
    /// the way that is null names nothing; a `member[i]` step names the
    /// target in each of the [`FILLED`] elements.
    fn resolve(
        &self,
        container: &Container<'_>,
        path: &str,
        owner: &str,
    ) -> Result<Vec<(*mut u8, usize)>> {
        let segments: Vec<&str> = path.split("->").flat_map(|s| s.split('.')).collect();
        let (first, mut indexed) = split_index(segments[0]);
        let index = container
            .vars
            .iter()
            .position(|v| v.name == first)
            .ok_or_else(|| {
                FillError(format!(
                    "{owner}: length path `{path}` starts at unknown member `{first}`"
                ))
            })?;
        let mut var: &Variable = &container.vars[index];
        let mut places = vec![container.storage[index]];
        for segment in &segments[1..] {
            let (segment, next_indexed) = split_index(segment);
            let layout = self
                .layouts
                .of_aggregate(var.ty.base)
                .map_err(|e| FillError(format!("{owner}: {path}: {e}")))?;
            let element = self.size_of_base(var.ty.base, path)?;
            let ty = &self.an.reg.types[var.ty.base];
            let member = ty
                .variables
                .iter()
                .position(|v| v.name == segment)
                .ok_or_else(|| {
                    FillError(format!(
                        "{owner}: `{path}`: {} has no member `{segment}`",
                        ty.name
                    ))
                })?;
            let mut next = Vec::new();
            for &place in &places {
                let mut base = place;
                if var.ty.is_pointer() {
                    // SAFETY: a pointer member holds a pointer.
                    base = unsafe { place.cast::<*mut u8>().read_unaligned() };
                    if base.is_null() {
                        continue;
                    }
                }
                for i in 0..if indexed { FILLED } else { 1 } {
                    // SAFETY: the element and the offset are inside what was allocated.
                    next.push(unsafe { base.add(i * element + layout.offsets[member] as usize) });
                }
            }
            places = next;
            var = &ty.variables[member];
            indexed = next_indexed;
        }
        let size = self.size_of_base(var.ty.base, path)?;
        let mut targets = Vec::new();
        for place in places {
            let mut target = place;
            if var.ty.is_pointer() {
                // SAFETY: a pointer member holds a pointer.
                target = unsafe { place.cast::<*mut u8>().read_unaligned() };
                if target.is_null() {
                    continue;
                }
            }
            targets.push((target, size));
        }
        Ok(targets)
    }
}

/// `name[i]` into the name and whether it was indexed.
fn split_index(segment: &str) -> (&str, bool) {
    let segment = segment.trim();
    match segment.strip_suffix("[i]") {
        Some(name) => (name, true),
        None => (segment, false),
    }
}

/// Writes the low `size` bytes of `value`, little-endian, which is the layout
/// of an integer of that size on the hosts the parity test runs on.
///
/// # Safety
/// `at` must point to `size` writable bytes.
unsafe fn write_int(at: *mut u8, size: usize, value: u64) {
    let bytes = value.to_le_bytes();
    // SAFETY: by the contract, and `size <= 8`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, size.min(8)) };
}
