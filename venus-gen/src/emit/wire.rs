//! The wire walks: how one variable is written to, and read back from, the
//! command stream. This is `vn_protocol.py`'s `VariableInfo` and the
//! `_encode_variable`/`_decode_variable` family, producing Rust token trees.
//!
//! One walk serves both `sizeof` and `encode`: the generated functions are
//! generic over a `Sink`, and the sizing sink counts what the encoding sink
//! would write, so the two cannot disagree.

use proc_macro2::{Ident, Literal, TokenStream};
use quote::{format_ident, quote};

use super::names::{Level, Shape, base_type, func, ident};
use crate::analysis::Analysis;
use crate::expr::{self, Expr, Op};
use crate::registry::{Category, Len, RegistryError, TypeId, Variable};

type Result<T> = std::result::Result<T, RegistryError>;

/// `VariableInfo.VALID` and friends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validity {
    /// Initialized, such as an input to the driver.
    Valid,
    /// Uninitialized, such as an output of the driver.
    Invalid,
    /// Partially initialized: a handle (the driver chose its id) or a struct
    /// (its sType, pNext and handles).
    Partial,
}

/// How a member of the container is reached from the generated function.
#[derive(Clone, Copy, Debug)]
pub enum Access {
    /// `(*val).member`
    Val,
    /// A parameter of the generated function, named after the member.
    Param,
}

/// What a generated function walks: a struct, union or command.
#[derive(Clone, Copy)]
pub struct Frame {
    pub container: TypeId,
    pub access: Access,
}

/// A variable's place, with the levels of indirection not yet consumed.
#[derive(Clone)]
struct Place {
    expr: TokenStream,
    /// `expr` is a dereference, which needs parentheses before a postfix
    /// operator.
    is_deref: bool,
    base: TypeId,
    levels: Vec<Level>,
}

impl Place {
    /// The place of element `index` of the outermost level.
    fn element(&self, index: &Ident) -> Result<Place> {
        let (first, rest) = self
            .levels
            .split_first()
            .ok_or_else(|| RegistryError::unsupported("indexing a place that is not an array"))?;
        let inner = &self.expr;
        let inner = if self.is_deref {
            quote!((#inner))
        } else {
            quote!(#inner)
        };
        let (expr, is_deref) = match first {
            Level::Ptr { .. } => (quote!(*#inner.add(#index)), true),
            Level::Array { .. } => (quote!(#inner[#index]), false),
        };
        Ok(Place {
            expr,
            is_deref,
            base: self.base,
            levels: rest.to_vec(),
        })
    }

    /// A pointer to what the remaining levels leave.
    fn pointer(&self, an: &Analysis, mutable: bool) -> Result<TokenStream> {
        let expr = &self.expr;
        let base = base_type(an, self.base)?;
        Ok(match self.levels.as_slice() {
            [] if mutable => quote!(::core::ptr::addr_of_mut!(#expr)),
            [] => quote!(::core::ptr::addr_of!(#expr)),
            [Level::Ptr { .. }] if mutable => quote!(PtrExt::into_mut(#expr)),
            [Level::Ptr { .. }] => quote!(PtrExt::into_const(#expr)),
            [Level::Array { .. }] if mutable => {
                quote!(::core::ptr::addr_of_mut!(#expr).cast::<#base>())
            }
            [Level::Array { .. }] => quote!(::core::ptr::addr_of!(#expr).cast::<#base>()),
            _ => {
                return Err(RegistryError::unsupported(
                    "a pointer to a place with more than one level of indirection left",
                ));
            }
        })
    }
}

/// What the element functions are called on after the loops.
#[derive(Clone, Copy, Debug)]
enum Stem {
    /// `void*` with a length.
    Blob,
    /// `char` array or string.
    Chars,
    /// A scalar, or (with `array_size`) an array of scalars.
    Scalar(TypeId),
    /// A struct, union or handle, named by its type.
    Named(TypeId),
}

struct Loop {
    count: TokenStream,
    /// The counter's name, `i`, `j`, ...
    iter: Ident,
    /// The count evaluated once, `n0`, `n1`, ...
    bound: Ident,
}

struct Plan {
    place: Place,
    loops: Vec<Loop>,
    /// The unrolled innermost loop of a scalar array.
    array_size: Option<TokenStream>,
    stem: Stem,
    selector: Option<TokenStream>,
    c_string: bool,
}

pub struct Wire<'a> {
    pub an: &'a Analysis,
}

fn u64_literal(value: u64) -> TokenStream {
    let literal = Literal::u64_suffixed(value);
    quote!(#literal)
}

impl<'a> Wire<'a> {
    pub fn new(an: &'a Analysis) -> Self {
        Self { an }
    }

    fn category(&self, id: TypeId) -> Category {
        self.an.reg.types[id].category()
    }

    fn member_place(&self, frame: Frame, var: &Variable) -> Result<Place> {
        let shape = Shape::of(self.an, &var.ty, &var.name)?;
        let name = ident(&var.name);
        let expr = match frame.access {
            Access::Val => quote!((*val).#name),
            Access::Param => quote!(#name),
        };
        Ok(Place {
            expr,
            is_deref: false,
            base: shape.base,
            levels: shape.levels,
        })
    }

    /// `Gen._get_variable_validity`.
    pub fn validity(&self, container: TypeId, var: &Variable, initialized: bool) -> Validity {
        if initialized {
            return Validity::Valid;
        }
        let ty = &self.an.reg.types[container];
        for other in &ty.variables {
            if other.name == var.name || other.attrs.static_len.is_some() {
                continue;
            }
            for len in &other.attrs.lens {
                // Only the first hop of the path is a variable of `container`.
                if let Some(found) = self.find_variables(container, &len.name)
                    && found.first() == Some(&var.name)
                {
                    return Validity::Valid;
                }
            }
        }
        match self.category(var.ty.base) {
            Category::Handle | Category::Struct => Validity::Partial,
            _ => Validity::Invalid,
        }
    }

    /// `VkType.find_variables`: the variables a length path walks through, as
    /// names with the container of each.
    fn find_variables(&self, container: TypeId, path: &str) -> Option<Vec<String>> {
        let names: Vec<&str> = if path.contains("[i].") {
            path.split("[i].").collect()
        } else {
            path.split("->").collect()
        };
        let mut found = Vec::new();
        let mut current = container;
        for name in names {
            let var = self.an.reg.types[current]
                .variables
                .iter()
                .find(|v| v.name == name)?;
            found.push(var.name.clone());
            current = var.ty.base;
        }
        Some(found)
    }

    // -- counts ---------------------------------------------------------------

    /// The Rust value (as `u64`) of the member path a length names.
    fn path_value(&self, frame: Frame, path: &str, var_name: &str) -> Result<TokenStream> {
        let hops: Vec<(&str, bool)> = if path.contains("[i].") {
            path.split("[i].").map(|name| (name, true)).collect()
        } else {
            path.split("->").map(|name| (name, false)).collect()
        };
        let mut current = frame.container;
        let mut value: Option<(TokenStream, bool, TypeId, bool)> = None;
        let mut first_ptr: Option<TokenStream> = None;
        for (index, (name, indexed)) in hops.iter().enumerate() {
            let var = self.an.reg.types[current]
                .variables
                .iter()
                .find(|v| v.name == *name)
                .ok_or_else(|| {
                    RegistryError::inconsistent(format!(
                        "length path `{path}` of {var_name} names no member `{name}`"
                    ))
                })?;
            let member = ident(name);
            let expr = match (&value, index) {
                (None, _) => match frame.access {
                    Access::Val => quote!((*val).#member),
                    Access::Param => quote!(#member),
                },
                (Some((cur, _, _, _)), _) if *indexed => {
                    // `a[i].b`: element `i` of the pointer `a`, then `b`.
                    quote!((*#cur.add(i)).#member)
                }
                (Some((cur, _, _, _)), _) => quote!((*#cur).#member),
            };
            let is_ptr = var.ty.is_pointer();
            if index == 0 && is_ptr {
                first_ptr = Some(expr.clone());
            }
            current = var.ty.base;
            value = Some((expr, is_ptr, var.ty.base, var.ty.is_static_array()));
        }
        let (expr, is_ptr, base, is_array) = value.expect("a path has a member");
        let expr = if is_ptr { quote!((*#expr)) } else { expr };
        if is_array {
            return Err(RegistryError::unsupported(format!(
                "length path `{path}` of {var_name} ends in an array"
            )));
        }
        let as_u64 = match self.category(base) {
            Category::Enum => quote!(#expr.0 as u64),
            _ => quote!(#expr as u64),
        };
        // `(first ? expr : 0)` when the path goes through a pointer.
        let through_pointer = hops.len() > 1 || is_ptr;
        Ok(match first_ptr {
            Some(root) if through_pointer && var_name != "ppBuildRangeInfos" => {
                quote!(len_if_set(PtrExt::into_const(#root), || #as_u64))
            }
            _ => as_u64,
        })
    }

    /// A length expression as `u64` tokens. `nested` operands of an
    /// operator are parenthesized; the outermost one is not, so it can stand
    /// as a statement's value.
    fn expr_tokens(
        &self,
        frame: Frame,
        expr: &Expr,
        len: &Len,
        var_name: &str,
        nested: bool,
    ) -> Result<TokenStream> {
        Ok(match expr {
            Expr::Int(value) => u64_literal(*value),
            Expr::Const(name) => {
                let value = u64::try_from(self.an.reg.constant(name)?).map_err(|_| {
                    RegistryError::unsupported(format!("constant {name} is negative"))
                })?;
                u64_literal(value)
            }
            Expr::Path => self.path_value(frame, &len.name, var_name)?,
            // The parentheses of the source matter only for grouping, which
            // `nested` already carries.
            Expr::Paren(inner) => self.expr_tokens(frame, inner, len, var_name, nested)?,
            Expr::Bin(op, left, right) => {
                let left = self.expr_tokens(frame, left, len, var_name, true)?;
                let right = self.expr_tokens(frame, right, len, var_name, true)?;
                let tokens = match op {
                    Op::Add => quote!(#left + #right),
                    Op::Sub => quote!(#left - #right),
                    Op::Mul => quote!(#left * #right),
                    Op::Div => quote!(#left / #right),
                };
                if nested { quote!((#tokens)) } else { tokens }
            }
        })
    }

    // -- the plan of one variable ----------------------------------------------

    fn plan(&self, frame: Frame, var: &Variable) -> Result<Plan> {
        let place = self.member_place(frame, var)?;
        let mut plan = Plan {
            place,
            loops: Vec::new(),
            array_size: None,
            stem: Stem::Named(var.ty.base),
            selector: None,
            c_string: var.has_c_string(),
        };
        // `_init_func_stem`
        let base = var.ty.base;
        plan.stem = if crate::analysis::is_blob(&self.an.reg, var) {
            Stem::Blob
        } else {
            match self.category(base) {
                Category::Bitmask => Stem::Scalar(
                    self.an.reg.types[base]
                        .typedef
                        .expect("a bitmask is a typedef"),
                ),
                Category::Default if self.an.reg.types[base].name == "char" => Stem::Chars,
                Category::Default | Category::Basetype | Category::Enum => Stem::Scalar(base),
                _ => Stem::Named(base),
            }
        };
        // `_init_loop_info`
        let mut counts: Vec<TokenStream> = Vec::new();
        if var.attrs.static_len.is_some() || var.attrs.lens.is_empty() {
            if let Some(dim) = &var.ty.decor.dim {
                let len = crate::layout::Layouts::new(&self.an.reg, 8).array_len(dim, &var.name)?;
                counts.push(u64_literal(len));
            }
        } else {
            for (level, len) in var.attrs.lens.iter().enumerate() {
                if len.expr == "null-terminated" {
                    // The string is what is left after `level` loops.
                    let mut at = plan.place.clone();
                    for l in 0..level {
                        at = at.element(&loop_counter(l))?;
                    }
                    let pointer = at.pointer(self.an, false)?;
                    counts.push(quote!(cstr_size(#pointer)));
                } else {
                    let parsed = expr::parse(&len.expr, &len.name)?;
                    counts.push(self.expr_tokens(frame, &parsed, len, &var.name, false)?);
                }
            }
        }
        for (level, count) in counts.into_iter().enumerate() {
            plan.loops.push(Loop {
                count,
                iter: loop_counter(level),
                bound: format_ident!("n{}", level),
            });
        }
        // `_unroll_loop`: scalar arrays are written whole, to get padding right.
        let scalar = matches!(
            self.category(base),
            Category::Default | Category::Basetype | Category::Enum
        );
        if scalar && let Some(last) = plan.loops.pop() {
            plan.array_size = Some(last.count);
        }
        if let Some(selector) = &var.attrs.selector {
            let sibling = self.an.reg.types[frame.container]
                .variables
                .iter()
                .find(|v| v.name == *selector)
                .ok_or_else(|| {
                    RegistryError::inconsistent(format!(
                        "selector `{selector}` of {} names no member",
                        var.name
                    ))
                })?;
            let place = self.member_place(frame, sibling)?;
            let expr = place.expr;
            plan.selector = Some(quote!(#expr));
        }
        Ok(plan)
    }

    /// The place after all loops, ready to be passed to an element function.
    fn element_place(&self, plan: &Plan) -> Result<Place> {
        let mut place = plan.place.clone();
        for l in &plan.loops {
            place = place.element(&l.iter)?;
        }
        Ok(place)
    }

    fn named_suffix(&self, validity: Validity, id: TypeId, partial_ok: bool) -> &'static str {
        if partial_ok && validity == Validity::Partial && self.category(id) == Category::Struct {
            "_partial"
        } else {
            ""
        }
    }

    // -- encode ---------------------------------------------------------------

    /// The statement that encodes the element a plan ends on.
    fn encode_call(
        &self,
        plan: &Plan,
        validity: Validity,
        stride: Option<&TokenStream>,
    ) -> Result<TokenStream> {
        let place = self.element_place(plan)?;
        let mut pointer = place.pointer(self.an, false)?;
        if let (Some(stride), Some(last)) = (stride, plan.loops.last()) {
            // Elements are `stride` bytes apart, whatever their size is.
            let outer = {
                let mut at = plan.place.clone();
                for l in &plan.loops[..plan.loops.len() - 1] {
                    at = at.element(&l.iter)?;
                }
                at.pointer(self.an, false)?
            };
            let iter = &last.iter;
            let base = base_type(self.an, plan.place.base)?;
            pointer = quote!(#outer.cast::<u8>().add(#stride as usize * #iter).cast::<#base>());
        }
        // The unrolled array's size, evaluated once into `array_size`.
        let size = plan
            .array_size
            .as_ref()
            .map(|_| quote!(array_size as usize));
        Ok(match plan.stem {
            Stem::Blob => {
                let size = size.ok_or_else(not_an_array)?;
                quote!(encode_blob_array(enc, #pointer.cast::<u8>(), #size);)
            }
            Stem::Chars => {
                let size = size.ok_or_else(not_an_array)?;
                quote!(encode_char_array(enc, #pointer, #size);)
            }
            Stem::Scalar(id) => {
                let ty = base_type(self.an, id)?;
                match size {
                    Some(size) => quote!(encode_array::<_, #ty>(enc, #pointer, #size);),
                    None => quote!(encode_scalar::<_, #ty>(enc, #pointer);),
                }
            }
            Stem::Named(id) => {
                let ty = &self.an.reg.types[id];
                let suffix = self.named_suffix(validity, id, true);
                let function = func("encode", &format!("{}{}", ty.name, suffix));
                match &plan.selector {
                    Some(selector) => quote!(#function(enc, #pointer, #selector);),
                    None => quote!(#function(enc, #pointer);),
                }
            }
        })
    }

    /// Wraps `call` in the plan's loops, each preceded by its array size.
    fn encode_loops(&self, plan: &Plan, call: TokenStream) -> TokenStream {
        let mut body = call;
        if let Some(size) = &plan.array_size {
            body = quote! {
                let array_size: u64 = #size;
                encode_array_size(enc, array_size);
                #body
            };
        }
        for l in plan.loops.iter().rev() {
            let (count, iter, bound) = (&l.count, &l.iter, &l.bound);
            body = quote! {
                let #bound: u64 = #count;
                encode_array_size(enc, #bound);
                for #iter in 0..#bound as usize {
                    #body
                }
            };
        }
        quote!({ #body })
    }

    /// `_encode_variable_info` and `_encode_variable`.
    pub fn encode_var(
        &self,
        frame: Frame,
        var: &Variable,
        validity: Validity,
    ) -> Result<TokenStream> {
        let name = &var.name;
        let is_array = var.is_dynamic_array();
        let is_pointer = var.ty.is_pointer();
        let place = self.member_place(frame, var)?;
        let value = place.expr.clone();
        if !self.an.is_serializable(var) {
            // Fine when the member is optional or in a union that is encoded
            // with a default tag: it is never selected, or it is null.
            let latch = quote!(enc.fail(EncodeError::NotSerializable { member: #name }););
            return Ok(if is_array {
                quote! {
                    if !#value.is_null() { #latch } else { encode_array_size(enc, 0); }
                }
            } else if is_pointer {
                quote! {
                    if encode_simple_pointer(enc, #value.into_const()) { #latch }
                }
            } else {
                latch
            });
        }
        let plan = self.plan(frame, var)?;
        if validity == Validity::Invalid {
            return Ok(if is_array {
                let size = plan.array_size.clone().ok_or_else(|| {
                    RegistryError::unsupported(format!(
                        "output array {name} is not an array of scalars"
                    ))
                })?;
                quote! {
                    encode_array_size(enc, if #value.is_null() { 0 } else { #size });
                }
            } else if is_pointer {
                quote!(encode_simple_pointer(enc, #value.into_const());)
            } else {
                TokenStream::new()
            });
        }
        let stride = var
            .attrs
            .stride
            .as_ref()
            .map(|stride| ident(stride))
            .map(|stride| quote!(#stride));
        let call = self.encode_call(&plan, validity, stride.as_ref())?;
        let body = self.encode_loops(&plan, call);
        let condition = match var.attrs.condition {
            Some(condition) => condition_tokens(condition),
            None => quote!(!#value.is_null()),
        };
        Ok(if is_array {
            quote! {
                if #condition #body else { encode_array_size(enc, 0); }
            }
        } else if is_pointer {
            quote! {
                if encode_simple_pointer(enc, #value.into_const()) #body
            }
        } else {
            body
        })
    }

    // -- decode ---------------------------------------------------------------

    fn decode_call(&self, plan: &Plan, count: Option<&Ident>) -> Result<TokenStream> {
        let place = self.element_place(plan)?;
        let pointer = place.pointer(self.an, true)?;
        Ok(match plan.stem {
            Stem::Blob => {
                let size = count.ok_or_else(not_an_array)?;
                quote!(decode_blob_array(dec, #pointer.cast::<u8>(), #size as usize);)
            }
            Stem::Chars => {
                let size = count.ok_or_else(not_an_array)?;
                quote!(decode_char_array(dec, #pointer, #size as usize);)
            }
            Stem::Scalar(id) => {
                let ty = base_type(self.an, id)?;
                match count {
                    Some(size) => quote!(decode_array::<#ty>(dec, #pointer, #size as usize);),
                    None => quote!(decode_scalar::<#ty>(dec, #pointer);),
                }
            }
            Stem::Named(id) => {
                let ty = &self.an.reg.types[id];
                let function = func("decode", &ty.name);
                quote!(#function(dec, #pointer);)
            }
        })
    }

    /// `_decode_variable_info` (without storage allocation) and
    /// `_decode_variable`. Everything the driver decodes is `Valid`.
    pub fn decode_var(&self, frame: Frame, var: &Variable) -> Result<TokenStream> {
        let name = &var.name;
        let is_array = var.is_dynamic_array();
        let is_pointer = var.ty.is_pointer();
        if !self.an.is_serializable(var) {
            return Ok(quote!(dec.fail(DecodeError::NotSerializable { member: #name });));
        }
        let plan = self.plan(frame, var)?;
        // Decode the array sizes, checking each against the expected count.
        let mut array_count: Option<Ident> = None;
        let mut body = TokenStream::new();
        if let Some(size) = &plan.array_size {
            let decl = if plan.c_string {
                quote!(let array_size: u64 = decode_array_size_unchecked(dec);)
            } else {
                quote!(let array_size: u64 = decode_array_size(dec, #size);)
            };
            body = decl;
            array_count = Some(format_ident!("array_size"));
        }
        let call = self.decode_call(&plan, array_count.as_ref())?;
        body = quote!(#body #call);
        for l in plan.loops.iter().rev() {
            let (count, iter, bound) = (&l.count, &l.iter, &l.bound);
            body = quote! {
                let #bound: u64 = decode_array_size(dec, #count);
                for #iter in 0..#bound as usize {
                    #body
                }
            };
        }
        Ok(if is_array {
            quote! {
                if peek_array_size(dec) != 0 { #body } else { decode_array_size_unchecked(dec); }
            }
        } else if is_pointer {
            quote!(if decode_simple_pointer(dec) { #body })
        } else {
            quote!({ #body })
        })
    }

    // -- unions ---------------------------------------------------------------

    /// `get_union_cases`: the selector values of each member, or its index.
    pub fn union_cases(&self, id: TypeId) -> Result<Vec<(TokenStream, usize)>> {
        let ty = &self.an.reg.types[id];
        let mut cases = Vec::new();
        if ty.is_valid_union() {
            for (index, var) in ty.variables.iter().enumerate() {
                for selection in &var.attrs.selection {
                    let constant = ident(selection);
                    cases.push((quote!(#constant), index));
                }
            }
        } else {
            for index in 0..ty.variables.len() {
                let literal = Literal::u32_suffixed(u32::try_from(index).expect("few members"));
                cases.push((quote!(#literal), index));
            }
        }
        Ok(cases)
    }
}

fn not_an_array() -> RegistryError {
    RegistryError::unsupported("a blob or a string without a length")
}

/// The loop counter of nesting `level`: `i`, `j`, ...
fn loop_counter(level: usize) -> Ident {
    let name = char::from(b'i' + u8::try_from(level).expect("shallow loops"));
    format_ident!("{}", name.to_string())
}

fn condition_tokens(condition: crate::policy::Condition) -> TokenStream {
    use crate::policy::Condition;
    match condition {
        Condition::Equals { member, value } => {
            let member = ident(member);
            let value = ident(value);
            quote!((*val).#member == #value)
        }
        Condition::FlagClear { member, bit } => {
            let member = ident(member);
            let bit = ident(bit);
            quote!(((*val).#member & (#bit.0 as u32)) == 0)
        }
    }
}
