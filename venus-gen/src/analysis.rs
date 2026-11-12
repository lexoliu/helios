//! What `vn_protocol.py`'s `Gen` decides before it writes any code: which
//! types and commands are supported and serializable, how each parameter is
//! classified, and the pNext conditions.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::policy;
use crate::registry::{
    Category, Registry, RegistryError, TypeDef, TypeId, TypeRef, Variable, top_level_split,
};

type Result<T> = std::result::Result<T, RegistryError>;

/// The result of the analysis over a [`Registry`].
pub struct Analysis {
    pub reg: Registry,
    /// Supported types by category, in registry order (`Gen.supported_types`).
    pub supported: HashMap<Category, Vec<TypeId>>,
    supported_set: HashSet<TypeId>,
    /// Memo of [`Analysis::is_serializable`]; filled while it recurses.
    serializable: RefCell<HashMap<TypeId, bool>>,
    /// Structs and unions that are reachable from a serializable command
    /// through serializable types, and the commands themselves.
    pub generated_structs: Vec<TypeId>,
    pub generated_commands: Vec<TypeId>,
    pub skipped_structs: Vec<TypeId>,
    pub skipped_commands: Vec<TypeId>,
    /// Extension name to its entry, for the extensions in the policy list.
    ext_by_name: HashMap<String, usize>,
}

/// A renderer-protocol extension test, `build_ext_deps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtExpr {
    Has(u32),
    And(Vec<ExtExpr>),
    Or(Vec<ExtExpr>),
}

/// The condition under which a chained struct is dropped from a pNext chain
/// (`get_type_condition`): the struct is skipped when the renderer protocol
/// does not support what introduced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainCondition {
    /// Skipped unless the renderer's `vk.xml` is at least this API version.
    ApiVersion { major: u32, minor: u32 },
    /// Skipped while none of the following holds: every `exts` entry
    /// supported... as the Python spells it, `!(a) && !(b) && !(pair)`.
    Extensions {
        /// Extension numbers, each tested as `!has_extension(n)`.
        exts: Vec<u32>,
        /// Extension numbers paired with the extra dependency they need,
        /// each tested as `!(has_extension(n) && deps)`.
        pairs: Vec<(u32, ExtExpr)>,
    },
}

impl Analysis {
    pub fn new(mut reg: Registry) -> Result<Self> {
        fixup_registry(&mut reg)?;
        let mut ext_by_name = HashMap::new();
        for (index, ext) in reg.extensions.iter().enumerate() {
            if policy::EXTENSION_LIST.contains(&ext.name.as_str()) {
                ext_by_name.insert(ext.name.clone(), index);
            }
        }
        let mut analysis = Self {
            reg,
            supported: HashMap::new(),
            supported_set: HashSet::new(),
            serializable: RefCell::new(HashMap::new()),
            generated_structs: Vec::new(),
            generated_commands: Vec::new(),
            skipped_structs: Vec::new(),
            skipped_commands: Vec::new(),
            ext_by_name,
        };
        analysis.init_supported_types();
        analysis.check_command_types()?;
        analysis.collect_generated()?;
        Ok(analysis)
    }

    pub fn is_supported(&self, ty: TypeId) -> bool {
        self.supported_set.contains(&ty)
    }

    fn init_supported_types(&mut self) {
        let mut types: Vec<TypeId> = Vec::new();
        for feature in &self.reg.features {
            types.extend(&feature.types);
        }
        for ext in &self.reg.extensions {
            if !policy::EXTENSION_LIST.contains(&ext.name.as_str()) {
                continue;
            }
            types.extend(&ext.types);
            for (key, optional) in &ext.optional_types {
                if support_type_depends(key) {
                    types.extend(optional);
                }
            }
        }
        let mut with_deps = HashSet::new();
        for ty in types {
            if !with_deps.contains(&ty) {
                let mut deps = Vec::new();
                self.add_deps(ty, &mut deps, &mut HashSet::new());
                with_deps.extend(deps);
            }
        }
        // Filter p_next to supported types.
        for id in 0..self.reg.types.len() {
            if with_deps.contains(&id) {
                let filtered: Vec<TypeId> = self.reg.types[id]
                    .p_next
                    .iter()
                    .copied()
                    .filter(|t| with_deps.contains(t))
                    .collect();
                self.reg.types[id].p_next = filtered;
            }
        }
        // Registry order.
        for id in 0..self.reg.types.len() {
            if !with_deps.contains(&id) {
                continue;
            }
            let category = self.reg.types[id].category();
            let list = self.supported.entry(category).or_default();
            if !list.contains(&id) {
                list.push(id);
            }
        }
        self.supported_set = with_deps;
    }

    /// `VkType._add_deps`.
    fn add_deps(&self, id: TypeId, deps: &mut Vec<TypeId>, seen: &mut HashSet<TypeId>) {
        if !seen.insert(id) {
            return;
        }
        let ty = &self.reg.types[id];
        if let Some(typedef) = ty.typedef {
            self.add_deps(typedef, deps, seen);
        }
        if let Some(requires) = ty.requires {
            self.add_deps(requires, deps, seen);
        }
        if let Some(ret) = &ty.ret {
            self.add_deps(ret.base, deps, seen);
        }
        for var in &ty.variables {
            if var.ty.base != id {
                self.add_deps(var.ty.base, deps, seen);
            }
        }
        deps.push(id);
    }

    /// Every supported command must have its `VK_COMMAND_TYPE_<cmd>_EXT`
    /// value, and every value must belong to a supported command.
    fn check_command_types(&self) -> Result<()> {
        let command_type = self.reg.require("VkCommandTypeEXT")?;
        let enums = self.reg.types[command_type]
            .enums
            .as_ref()
            .expect("VkCommandTypeEXT is an enum");
        let mut count = 0;
        for &cmd in self
            .supported
            .get(&Category::Command)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            for name in self.reg.all_names(cmd) {
                let key = format!("VK_COMMAND_TYPE_{name}_EXT");
                if enums.get(&key).is_none() {
                    return Err(RegistryError::inconsistent(format!(
                        "{key} is not defined for VkCommandTypeEXT"
                    )));
                }
                count += 1;
            }
        }
        if count != enums.values.len() {
            return Err(RegistryError::inconsistent(format!(
                "VkCommandTypeEXT has {} values for {count} supported command names",
                enums.values.len()
            )));
        }
        Ok(())
    }

    /// `GenStructsAndCommands`: what is reachable from a supported command.
    fn collect_generated(&mut self) -> Result<()> {
        let commands = self
            .supported
            .get(&Category::Command)
            .cloned()
            .unwrap_or_default();
        let mut visited: HashSet<TypeId> = HashSet::new();
        for cmd in commands {
            self.add_generated(cmd, &mut visited)?;
        }
        Ok(())
    }

    fn add_generated(&mut self, id: TypeId, visited: &mut HashSet<TypeId>) -> Result<()> {
        let category = self.reg.types[id].category();
        if !matches!(
            category,
            Category::Command | Category::Struct | Category::Union
        ) {
            return Ok(());
        }
        if !visited.insert(id) {
            return Ok(());
        }
        if self.is_serializable_type(id) {
            let ty = &self.reg.types[id];
            let mut deps: Vec<TypeId> = ty.variables.iter().map(|v| v.ty.base).collect();
            deps.extend(&ty.p_next);
            if let Some(ret) = &ty.ret {
                deps.push(ret.base);
            }
            for dep in deps {
                self.add_generated(dep, visited)?;
            }
            if category == Category::Command {
                self.generated_commands.push(id);
            } else {
                self.generated_structs.push(id);
            }
        } else if category == Category::Union
            && self.reg.types[id]
                .variables
                .iter()
                .all(|var| self.is_serializable(var))
        {
            // The generator emits no helpers for a union that needs manual
            // serialization; the pinned registry has none.
            return Err(RegistryError::unsupported(format!(
                "union {} needs manual serialization",
                self.reg.types[id].name
            )));
        } else if category == Category::Command {
            self.skipped_commands.push(id);
        } else {
            self.skipped_structs.push(id);
        }
        Ok(())
    }

    // -- serializability ----------------------------------------------------

    /// `is_serializable` of a type used without a variable around it.
    pub fn is_serializable_type(&self, id: TypeId) -> bool {
        let var = Variable {
            name: "unnamed".into(),
            ty: TypeRef::plain(id),
            attrs: Default::default(),
        };
        self.is_serializable(&var)
    }

    /// `Gen.is_serializable(var)`.
    pub fn is_serializable(&self, var: &Variable) -> bool {
        let mut id = var.ty.base;
        if self.reg.types[id].category() == Category::Basetype {
            match self.reg.types[id].typedef {
                Some(typedef) => id = typedef,
                None => return false,
            }
        }
        let ty = &self.reg.types[id];
        match ty.category() {
            Category::Include | Category::Define | Category::Funcpointer => return false,
            Category::Default => {
                let name = ty.name.as_str();
                return if policy::PRIMITIVE_TYPES.iter().any(|(n, _)| *n == name)
                    || name == "char"
                    || name == "size_t"
                {
                    true
                } else if name == "void" {
                    is_blob(&self.reg, var)
                } else {
                    false
                };
            }
            Category::Handle | Category::Enum | Category::Bitmask => return true,
            Category::Union => {
                return ty.is_valid_union()
                    || policy::UNION_DEFAULT_TAGS
                        .iter()
                        .any(|(n, _)| *n == ty.name);
            }
            Category::Basetype => unreachable!("resolved above"),
            Category::Struct | Category::Command => {}
        }
        if let Some(&known) = self.serializable.borrow().get(&id) {
            return known;
        }
        let result = self.struct_or_command_serializable(id);
        self.serializable.borrow_mut().insert(id, result);
        result
    }

    fn struct_or_command_serializable(&self, id: TypeId) -> bool {
        let (category, name, ret, variables) = {
            let ty = &self.reg.types[id];
            (
                ty.category(),
                ty.name.clone(),
                ty.ret.clone(),
                ty.variables.clone(),
            )
        };
        match category {
            Category::Struct => {
                if name == "VkBaseInStructure" || name == "VkBaseOutStructure" {
                    return false;
                }
            }
            Category::Command => {
                if policy::COMMAND_BLOCK_LIST.contains(&name.as_str()) {
                    return false;
                }
                if let Some(ret) = ret {
                    let ret_var = Variable {
                        name: "ret".into(),
                        ty: ret,
                        attrs: Default::default(),
                    };
                    if !self.is_serializable(&ret_var) {
                        return false;
                    }
                }
            }
            _ => unreachable!(),
        }
        // Break recursion through self-referencing pointers explicitly.
        self.serializable.borrow_mut().insert(id, true);
        for var in &variables {
            if var.maybe_null() || var.is_p_next() {
                continue;
            }
            if !self.is_serializable(var) {
                return false;
            }
        }
        true
    }

    /// `Gen.get_chain`: the serializable and the skipped structs a chain may
    /// carry.
    pub fn chain(&self, id: TypeId) -> (Vec<TypeId>, Vec<TypeId>) {
        let mut serializable = Vec::new();
        let mut skipped = Vec::new();
        for &next in &self.reg.types[id].p_next {
            if self.is_serializable_type(next) {
                serializable.push(next);
            } else {
                skipped.push(next);
            }
        }
        (serializable, skipped)
    }

    // -- chain conditions ----------------------------------------------------

    /// `Gen.build_ext_deps`.
    fn build_ext_deps(&self, deps: &str) -> Result<Option<ExtExpr>> {
        if !deps.contains('+') && !deps.contains(',') {
            let index = self.ext_by_name.get(deps).ok_or_else(|| {
                RegistryError::inconsistent(format!(
                    "dependency {deps} is not a supported extension"
                ))
            })?;
            return Ok(Some(ExtExpr::Has(self.reg.extensions[*index].number)));
        }
        let mut or_list = Vec::new();
        for dep in top_level_split(deps, ',') {
            let mut and_list = Vec::new();
            for and_dep in top_level_split(&dep, '+') {
                let mut s = and_dep.as_str();
                if let Some(r) = s.strip_prefix('(') {
                    s = r;
                }
                if let Some(r) = s.strip_suffix(')') {
                    s = r;
                }
                if let Some(converted) = self.build_ext_deps(s)? {
                    and_list.push(converted);
                }
            }
            if and_list.is_empty() {
                return Ok(None);
            }
            or_list.push(if and_list.len() > 1 {
                ExtExpr::And(and_list)
            } else {
                and_list.remove(0)
            });
        }
        Ok(Some(if or_list.len() > 1 {
            ExtExpr::Or(or_list)
        } else {
            or_list.remove(0)
        }))
    }

    /// `Gen.get_type_condition`.
    pub fn type_condition(&self, id: TypeId) -> Result<Option<ChainCondition>> {
        #[derive(PartialEq)]
        enum Cond {
            None,
            Api,
            Ext,
        }
        let mut cond = Cond::None;
        let mut api = (0, 0);
        for feature in &self.reg.features {
            if feature.types.contains(&id) {
                if ["1.0", "1.1", "1.2"].contains(&feature.number.as_str()) {
                    if cond != Cond::None {
                        return Err(RegistryError::inconsistent("a core type with a condition"));
                    }
                    return Ok(None);
                }
                let (major, minor) = feature.number.split_once('.').ok_or_else(|| {
                    RegistryError::inconsistent(format!("feature number {}", feature.number))
                })?;
                api = (parse_u32(major)?, parse_u32(minor)?);
                cond = Cond::Api;
                break;
            }
        }
        let mut exts = Vec::new();
        let mut pairs = Vec::new();
        for ext in &self.reg.extensions {
            if !self.ext_by_name.contains_key(&ext.name) {
                continue;
            }
            if ext.types.contains(&id) {
                if ext.name == "VK_MESA_venus_protocol" {
                    if cond != Cond::None {
                        return Err(RegistryError::inconsistent("a Venus type with a condition"));
                    }
                    return Ok(None);
                }
                exts.push(ext.number);
                cond = Cond::Ext;
            } else if !ext.optional_types.is_empty() {
                for (key, optional) in &ext.optional_types {
                    if support_type_depends(key) && optional.contains(&id) {
                        let deps = self.build_ext_deps(key)?.ok_or_else(|| {
                            RegistryError::inconsistent(format!(
                                "dependency {key} has no extension"
                            ))
                        })?;
                        pairs.push((ext.number, deps));
                        cond = Cond::Ext;
                        break;
                    }
                }
            }
        }
        match cond {
            Cond::None => Err(RegistryError::inconsistent(format!(
                "chained type {} belongs to no feature or extension",
                self.reg.types[id].name
            ))),
            Cond::Ext => Ok(Some(ChainCondition::Extensions { exts, pairs })),
            Cond::Api => Ok(Some(ChainCondition::ApiVersion {
                major: api.0,
                minor: api.1,
            })),
        }
    }
}

fn parse_u32(text: &str) -> Result<u32> {
    text.parse()
        .map_err(|_| RegistryError::inconsistent(format!("`{text}` is not a version number")))
}

/// `Gen.support_type_depends`.
pub fn support_type_depends(deps: &str) -> bool {
    if !deps.contains('+') && !deps.contains(',') {
        return policy::EXTENSION_LIST.contains(&deps);
    }
    for or_dep in top_level_split(deps, ',') {
        if !or_dep.contains('+') && policy::EXTENSION_LIST.contains(&or_dep.as_str()) {
            return true;
        }
        let mut support = true;
        for and_dep in top_level_split(&or_dep, '+') {
            let mut s = and_dep.as_str();
            if let Some(r) = s.strip_prefix('(') {
                s = r;
            }
            if let Some(r) = s.strip_suffix(')') {
                s = r;
            }
            if !support_type_depends(s) {
                support = false;
                break;
            }
        }
        if support {
            return true;
        }
    }
    false
}

/// `VkVariable.is_blob`.
pub fn is_blob(reg: &Registry, var: &Variable) -> bool {
    var.ty.indirection_depth() == 1
        && !var.ty.is_static_array()
        && reg.types[var.ty.base].name == "void"
        && var.has_len()
}

/// `Gen._fixup_registry` and `_set_type_needs`, for the driver.
fn fixup_registry(reg: &mut Registry) -> Result<()> {
    for id in 0..reg.types.len() {
        if reg.types[id].category == Some(Category::Command) {
            classify_command(&mut reg.types[id]);
        }
        set_type_needs(reg, id)?;
    }
    Ok(())
}

fn classify_command(ty: &mut TypeDef) {
    for var in &mut ty.variables {
        // Non-const pointers are considered outs.
        if var.ty.is_pointer() && !var.ty.is_const_pointer() {
            var.attrs.var_out = true;
        } else {
            var.attrs.var_in = true;
        }
    }
    // Outs named in another variable's length are in/out.
    let names: Vec<(Vec<String>, usize)> = ty
        .variables
        .iter()
        .enumerate()
        .map(|(i, v)| (v.attrs.lens.iter().map(|l| l.name.clone()).collect(), i))
        .collect();
    for (lens, _) in names {
        for name in lens {
            if let Some(found) = find_variable_path(&ty.variables, &name) {
                let var = &mut ty.variables[found];
                if var.attrs.var_out {
                    var.attrs.var_in = true;
                    var.attrs.var_out = true;
                }
            }
        }
    }
    // vkRegisterCustomBorderColorEXT::pIndex is in/out.
    if ty.name == "vkRegisterCustomBorderColorEXT" {
        for var in &mut ty.variables {
            if var.name == "pIndex" {
                var.attrs.var_in = true;
            }
        }
    }
}

/// `VkType.find_variables` restricted to the first path element of a command.
fn find_variable_path(variables: &[Variable], path: &str) -> Option<usize> {
    let first = path.split("->").next()?.split("[i].").next()?;
    variables.iter().position(|v| v.name == first)
}

fn set_type_needs(reg: &mut Registry, id: TypeId) -> Result<()> {
    let variables = reg.types[id].variables.clone();
    let name = reg.types[id].name.clone();
    let mut has_out = false;
    for (index, var) in variables.iter().enumerate() {
        if var.attrs.var_in {
            set_attribute(reg, var.ty.base, Need::Encode);
        }
        if var.attrs.var_out {
            set_attribute(reg, var.ty.base, Need::Partial);
            set_attribute(reg, var.ty.base, Need::Decode);
            has_out = true;
        }
        for ignore in policy::IGNORABLE_LIST {
            if ignore.struct_name == name && ignore.member == var.name {
                reg.types[id].variables[index].attrs.condition = Some(ignore.condition);
            }
        }
    }
    if let Some(ret) = reg.types[id].ret.clone() {
        set_attribute(reg, ret.base, Need::Decode);
        has_out = true;
    }
    if has_out {
        reg.types[id].has_out_ty = true;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Need {
    Encode,
    Decode,
    Partial,
}

/// `VkType.set_attribute`.
fn set_attribute(reg: &mut Registry, id: TypeId, need: Need) {
    let mut stack = vec![id];
    let mut seen: HashSet<TypeId> = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        if reg.types[id].name == "VkBaseOutStructure" {
            continue;
        }
        match need {
            Need::Encode => reg.types[id].need_encode = true,
            Need::Decode => reg.types[id].need_decode = true,
            Need::Partial => reg.types[id].need_partial = true,
        }
        let ty = &reg.types[id];
        stack.extend(ty.variables.iter().map(|v| v.ty.base));
        stack.extend(&ty.p_next);
    }
}
