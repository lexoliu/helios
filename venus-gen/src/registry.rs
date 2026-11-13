//! The registry model: `vk.xml` plus the two private XMLs, merged into one
//! type table, as `vkxml.py` (`VkRegistry`) builds it.
//!
//! Types live in an arena addressed by [`TypeId`]. A type name may be an
//! alias; every alias resolves to the same arena entry, exactly as the Python
//! type table maps both names to one object. Pointer and array decoration is
//! carried inline by a [`TypeRef`], not entered into the table.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use vk_parse::{
    Command, CommandDefinition, CommandParam, Enum as XmlEnum, EnumSpec, EnumsChild,
    ExtensionChild, InterfaceItem, Registry as XmlRegistry, RegistryChild, Type as XmlType,
    TypeCodeMarkup, TypeMember, TypeMemberMarkup, TypeSpec, TypesChild,
};

use crate::policy::{self, Condition};

/// Index of a type in [`Registry::types`].
pub type TypeId = usize;

/// Why the registry could not be built, or why a construct the generator has
/// no encoding for was reached.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("failed to parse {path}: {message}")]
    Parse { path: String, message: String },
    #[error("{path} reported parse errors: {errors}")]
    ParseErrors { path: String, errors: String },
    #[error("unsupported registry construct: {what}")]
    Unsupported { what: String },
    #[error("registry inconsistency: {what}")]
    Inconsistent { what: String },
}

impl RegistryError {
    pub(crate) fn unsupported(what: impl Into<String>) -> Self {
        Self::Unsupported { what: what.into() }
    }
    pub(crate) fn inconsistent(what: impl Into<String>) -> Self {
        Self::Inconsistent { what: what.into() }
    }
}

type Result<T> = std::result::Result<T, RegistryError>;

/// `VkType.category`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Include,
    Define,
    Default,
    Basetype,
    Handle,
    Enum,
    Bitmask,
    Struct,
    Union,
    Funcpointer,
    Command,
}

/// A parsed C declaration's decoration: `int* const* const blah[4]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Decor {
    /// The qualifier of the declared object itself (`const` in `int *const a`
    /// or `const int a`).
    pub qual: String,
    /// The text between the brackets of a static array.
    pub dim: Option<String>,
    /// A bit-field width.
    pub bit_size: Option<String>,
    /// One entry per `*`: the qualifier of what that pointer points to.
    pub ref_quals: Vec<String>,
}

/// A use of a type: the base type plus its decoration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeRef {
    pub base: TypeId,
    pub decor: Decor,
}

impl TypeRef {
    pub fn plain(base: TypeId) -> Self {
        Self {
            base,
            decor: Decor::default(),
        }
    }
    pub fn is_static_array(&self) -> bool {
        self.decor.dim.is_some()
    }
    pub fn is_pointer(&self) -> bool {
        !self.decor.ref_quals.is_empty()
    }
    pub fn indirection_depth(&self) -> usize {
        self.decor.ref_quals.len()
    }
    pub fn is_const_static_array(&self) -> bool {
        self.is_static_array() && self.decor.qual.contains("const")
    }
    pub fn is_const_pointer(&self) -> bool {
        self.decor.ref_quals.iter().any(|q| q.contains("const"))
    }
}

/// A length expression of a variable: the text, and the member path it
/// names (empty for `null-terminated`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Len {
    pub expr: String,
    pub name: String,
}

/// The attributes of a struct member, command parameter or return value.
#[derive(Clone, Debug, Default)]
pub struct VarAttrs {
    pub values: Vec<String>,
    pub lens: Vec<Len>,
    pub optional: Vec<String>,
    pub noautovalidity: bool,
    pub stride: Option<String>,
    pub selector: Option<String>,
    pub selection: Vec<String>,
    /// `wa_require_static_len`: the constant that sizes a static array.
    pub static_len: Option<String>,
    /// Set from [`policy::IGNORABLE_LIST`].
    pub condition: Option<Condition>,
    pub var_in: bool,
    pub var_out: bool,
}

/// `VkVariable`.
#[derive(Clone, Debug)]
pub struct Variable {
    pub name: String,
    pub ty: TypeRef,
    pub attrs: VarAttrs,
}

impl Variable {
    pub fn can_validate(&self) -> bool {
        !self.attrs.noautovalidity
    }
    pub fn is_optional(&self) -> bool {
        self.attrs.optional.first().is_some_and(|o| o == "true")
    }
    pub fn maybe_null(&self) -> bool {
        self.ty.is_pointer() && self.is_optional()
    }
    pub fn has_len(&self) -> bool {
        !self.attrs.lens.is_empty()
    }
    pub fn is_dynamic_array(&self) -> bool {
        self.ty.is_pointer() && self.has_len()
    }
    pub fn is_p_next(&self) -> bool {
        self.name == "pNext"
    }
    pub fn has_c_string(&self) -> bool {
        self.is_dynamic_array() && self.attrs.lens.iter().any(|l| l.expr == "null-terminated")
    }
}

/// `VkEnums`: the values of one enumeration, in definition order.
#[derive(Clone, Debug)]
pub struct Enums {
    pub bitwidth: u32,
    pub values: Vec<(String, String)>,
    /// The names `vk.xml` itself defines; the private XMLs extend the rest.
    pub vk_xml_values: Option<HashSet<String>>,
}

impl Enums {
    fn new() -> Self {
        Self {
            bitwidth: 32,
            values: Vec::new(),
            vk_xml_values: None,
        }
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn extend_value(&mut self, key: String, value: String) -> Result<()> {
        match self.values.iter().find(|(k, _)| *k == key) {
            Some((_, existing)) if *existing != value => Err(RegistryError::inconsistent(format!(
                "enum value {key} is defined as {existing} and as {value}"
            ))),
            Some(_) => Ok(()),
            None => {
                self.values.push((key, value));
                Ok(())
            }
        }
    }
}

/// `VkType`.
#[derive(Clone, Debug)]
pub struct TypeDef {
    pub name: String,
    /// `None` only while the type is a forward reference.
    pub category: Option<Category>,
    pub aliases: Vec<String>,
    pub is_private: bool,
    pub define: Option<String>,
    pub typedef: Option<TypeId>,
    pub dispatchable: bool,
    pub enums: Option<Enums>,
    pub requires: Option<TypeId>,
    pub s_type: Option<String>,
    pub(crate) struct_extends: Vec<String>,
    pub p_next: Vec<TypeId>,
    pub ret: Option<TypeRef>,
    pub can_device_lost: bool,
    pub variables: Vec<Variable>,
    pub returned_only: bool,
    /// For a struct or union: where each member of the C declaration, in
    /// declaration order, sits in `variables`. `variables` is in wire order
    /// (a length member precedes its array), which the memory layout does not
    /// follow.
    pub declared_order: Vec<usize>,
    /// The selector type of a union (`VkType.sty`).
    pub selector_type: Option<TypeRef>,
    // Set by the analysis (`Gen._set_type_needs`).
    pub need_encode: bool,
    pub need_decode: bool,
    pub need_partial: bool,
    pub has_out_ty: bool,
}

impl TypeDef {
    fn forward(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            category: None,
            aliases: Vec::new(),
            is_private: false,
            define: None,
            typedef: None,
            dispatchable: false,
            enums: None,
            requires: None,
            s_type: None,
            struct_extends: Vec::new(),
            p_next: Vec::new(),
            ret: None,
            can_device_lost: false,
            variables: Vec::new(),
            returned_only: false,
            declared_order: Vec::new(),
            selector_type: None,
            need_encode: false,
            need_decode: false,
            need_partial: false,
            has_out_ty: false,
        }
    }

    pub fn category(&self) -> Category {
        self.category
            .unwrap_or_else(|| panic!("type {} is referenced but never defined", self.name))
    }

    /// The members in the order the C declaration lists them, each with its
    /// index in `variables`; the order the memory layout follows.
    pub fn declared(&self) -> impl Iterator<Item = (usize, &Variable)> {
        self.declared_order.iter().map(|&i| (i, &self.variables[i]))
    }

    /// `VkType.is_valid_union`: every member names the selector values it
    /// is chosen by.
    pub fn is_valid_union(&self) -> bool {
        self.category == Some(Category::Union)
            && self.variables.iter().all(|v| !v.attrs.selection.is_empty())
    }
}

/// A feature (`VK_VERSION_x_y`) or extension and the types it requires.
#[derive(Clone, Debug)]
pub struct Feature {
    pub name: String,
    pub number: String,
    pub types: Vec<TypeId>,
}

/// An extension as the generator needs it.
#[derive(Clone, Debug)]
pub struct Extension {
    pub name: String,
    pub number: u32,
    pub supported: Vec<String>,
    pub version: u32,
    pub types: Vec<TypeId>,
    /// `<require depends=...>` blocks, keyed by the filtered dependency.
    pub optional_types: Vec<(String, Vec<TypeId>)>,
}

/// `VkRegistry`.
#[derive(Debug)]
pub struct Registry {
    pub types: Vec<TypeDef>,
    names: HashMap<String, TypeId>,
    pub features: Vec<Feature>,
    pub extensions: Vec<Extension>,
    /// Numeric values of the `API Constants`.
    pub constants: BTreeMap<String, String>,
    /// `VK_HEADER_VERSION_COMPLETE` as `[variant, major, minor, patch]`.
    pub vk_xml_version: [u32; 4],
    pub max_command_type_value: u64,
}

impl Registry {
    /// `VkRegistry.parse`: `vk.xml` first, then the private XMLs merged in.
    pub fn parse(vk_xml: &Path, private_xmls: &[&Path]) -> Result<Self> {
        let mut builder = Builder::new();
        builder.parse_xml(vk_xml)?;
        let vk_xml_types: HashSet<TypeId> = builder.reg.names.values().copied().collect();
        for ty in &mut builder.reg.types {
            if let Some(enums) = &mut ty.enums {
                enums.vk_xml_values = Some(enums.values.iter().map(|(k, _)| k.clone()).collect());
            }
        }
        for xml in private_xmls {
            builder.parse_xml(xml)?;
        }
        builder.resolve(&vk_xml_types)?;
        Ok(builder.reg)
    }

    pub fn ty(&self, id: TypeId) -> &TypeDef {
        &self.types[id]
    }

    pub fn lookup(&self, name: &str) -> Option<TypeId> {
        self.names.get(name).copied()
    }

    pub fn require(&self, name: &str) -> Result<TypeId> {
        self.lookup(name).ok_or_else(|| {
            RegistryError::inconsistent(format!("type {name} is not in the registry"))
        })
    }

    /// Names of `ty` and its aliases.
    pub fn all_names(&self, ty: TypeId) -> impl Iterator<Item = &str> {
        let def = &self.types[ty];
        std::iter::once(def.name.as_str()).chain(def.aliases.iter().map(String::as_str))
    }

    /// The numeric value of a (possibly aliased) enum constant.
    pub fn enum_value(&self, ty: TypeId, key: &str) -> Result<i128> {
        let enums = self.types[ty].enums.as_ref().ok_or_else(|| {
            RegistryError::inconsistent(format!("{} is not an enumeration", self.types[ty].name))
        })?;
        let text = enums.get(key).ok_or_else(|| {
            RegistryError::inconsistent(format!("{key} is not a value of {}", self.types[ty].name))
        })?;
        parse_int(text)
    }

    /// The value of an API constant or a plain integer literal.
    pub fn constant(&self, name: &str) -> Result<i128> {
        if let Ok(value) = parse_int(name) {
            return Ok(value);
        }
        let mut name = name;
        // Constants may alias other constants.
        for _ in 0..8 {
            let Some(text) = self.constants.get(name) else {
                return Err(RegistryError::unsupported(format!(
                    "array length {name} is not an API constant"
                )));
            };
            match parse_int(text) {
                Ok(value) => return Ok(value),
                Err(_) => name = text,
            }
        }
        Err(RegistryError::unsupported(format!(
            "constant {name} aliases too deeply"
        )))
    }
}

/// Parses a decimal or `0x` hexadecimal integer, optionally negative.
pub fn parse_int(text: &str) -> Result<i128> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let value = if let Some(hex) = digits.strip_prefix("0x") {
        i128::from_str_radix(hex, 16)
    } else {
        digits.parse::<i128>()
    }
    .map_err(|_| RegistryError::unsupported(format!("`{text}` is not an integer literal")))?;
    Ok(if negative { -value } else { value })
}

// ---------------------------------------------------------------------------
// Parsing.
// ---------------------------------------------------------------------------

struct Builder {
    reg: Registry,
    tags: Vec<String>,
    /// Enum extensions are applied while `<feature>`/`<extension>` blocks are
    /// read, possibly before the `<enums>` block of the extended type.
    pending_enum_values: Vec<(String, String, String)>,
}

fn is_vulkansc(api: &Option<String>) -> bool {
    api.as_deref() == Some("vulkansc")
}

/// `VkType._get_inner_text`-independent C declaration parser (`VkDecl.from_c`).
struct Decl {
    name: String,
    type_name: String,
    decor: Decor,
}

impl Decl {
    /// Parses `c_decl`, whose declared name is `name`. The XML text may have
    /// no separator between the `<type>` and the `<name>`, so the name is
    /// taken from the markup instead of being found by scanning.
    fn from_c(c_decl: &str, name: &str) -> Result<Self> {
        let mut c = c_decl;
        let mut bit_size = None;
        if let Some(index) = c.find(':') {
            bit_size = Some(c[index + 1..].trim().to_owned());
            c = &c[..index];
        }
        let mut dim = None;
        if let Some(index) = c.find('[') {
            let close = c.rfind(']').ok_or_else(|| {
                RegistryError::unsupported(format!("declaration `{c_decl}` has an unclosed `[`"))
            })?;
            dim = Some(c[index + 1..close].trim().to_owned());
            c = &c[..index];
        }
        let head = c.trim_end().strip_suffix(name).ok_or_else(|| {
            RegistryError::unsupported(format!("declaration `{c_decl}` does not end in `{name}`"))
        })?;
        let name = name.to_owned();
        let head = head.trim_end();
        let mut quals: Vec<&str> = head.split('*').collect();
        let mut words: Vec<&str> = quals[0].split_whitespace().collect();
        let type_name = words
            .pop()
            .ok_or_else(|| {
                RegistryError::unsupported(format!("declaration `{c_decl}` has no type"))
            })?
            .to_owned();
        let first = words.join(" ");
        quals[0] = &first;
        let mut ref_quals: Vec<String> = quals.iter().map(|q| q.trim().to_owned()).collect();
        let qual = ref_quals.pop().unwrap_or_default();
        Ok(Self {
            name,
            type_name,
            decor: Decor {
                qual,
                dim,
                bit_size,
                ref_quals,
            },
        })
    }
}

impl Builder {
    fn new() -> Self {
        Self {
            reg: Registry {
                types: Vec::new(),
                names: HashMap::new(),
                features: Vec::new(),
                extensions: Vec::new(),
                constants: BTreeMap::new(),
                vk_xml_version: [0; 4],
                max_command_type_value: 0,
            },
            tags: Vec::new(),
            pending_enum_values: Vec::new(),
        }
    }

    fn get_type(&mut self, name: &str) -> TypeId {
        if let Some(id) = self.reg.names.get(name) {
            return *id;
        }
        let id = self.reg.types.len();
        self.reg.types.push(TypeDef::forward(name));
        self.reg.names.insert(name.to_owned(), id);
        id
    }

    fn type_ref(&mut self, decl: &Decl) -> TypeRef {
        TypeRef {
            base: self.get_type(&decl.type_name),
            decor: decl.decor.clone(),
        }
    }

    fn init(&mut self, id: TypeId, name: &str, category: Category) -> Result<()> {
        let ty = &mut self.reg.types[id];
        if ty.category.is_some() {
            return Err(RegistryError::inconsistent(format!(
                "type {name} is defined twice"
            )));
        }
        ty.name = name.to_owned();
        ty.category = Some(category);
        Ok(())
    }

    fn parse_xml(&mut self, path: &Path) -> Result<()> {
        let display = path.display().to_string();
        let (registry, errors): (XmlRegistry, _) =
            vk_parse::parse_file(path).map_err(|error| RegistryError::Parse {
                path: display.clone(),
                message: format!("{error:?}"),
            })?;
        if !errors.is_empty() {
            return Err(RegistryError::ParseErrors {
                path: display,
                errors: format!("{errors:?}"),
            });
        }
        for child in &registry.0 {
            match child {
                RegistryChild::Tags(tags) => {
                    self.tags
                        .extend(tags.children.iter().map(|t| t.name.clone()));
                }
                RegistryChild::Types(types) => self.parse_types(types)?,
                RegistryChild::Enums(enums) => self.parse_enums(enums)?,
                RegistryChild::Commands(commands) => {
                    for command in &commands.children {
                        self.parse_command(command)?;
                    }
                }
                RegistryChild::Feature(feature) => self.parse_feature(feature)?,
                RegistryChild::Extensions(extensions) => {
                    for extension in &extensions.children {
                        self.parse_extension(extension)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn parse_types(&mut self, types: &vk_parse::Types) -> Result<()> {
        // Aliases first, in document order, then everything else.
        let mut all: Vec<&XmlType> = Vec::new();
        for child in &types.children {
            if let TypesChild::Type(ty) = child
                && !is_vulkansc(&ty.api)
            {
                all.push(ty);
            }
        }
        for ty in all.iter().filter(|t| t.alias.is_some()) {
            self.parse_alias(ty.name.as_deref(), ty.alias.as_deref())?;
        }
        for ty in all.iter().filter(|t| t.alias.is_none()) {
            self.parse_type(ty)?;
        }
        Ok(())
    }

    fn parse_alias(&mut self, name: Option<&str>, alias: Option<&str>) -> Result<()> {
        let (Some(name), Some(alias)) = (name, alias) else {
            return Err(RegistryError::unsupported(
                "an alias without name or target",
            ));
        };
        if self.reg.names.contains_key(name) {
            return Err(RegistryError::inconsistent(format!(
                "alias {name} names an existing type"
            )));
        }
        let id = self.get_type(alias);
        self.reg.types[id].aliases.push(name.to_owned());
        self.reg.names.insert(name.to_owned(), id);
        Ok(())
    }

    fn parse_type(&mut self, ty: &XmlType) -> Result<()> {
        let category = match ty.category.as_deref() {
            Some("include") => Category::Include,
            Some("define") => Category::Define,
            None => Category::Default,
            Some("basetype") => Category::Basetype,
            Some("handle") => Category::Handle,
            Some("enum") => Category::Enum,
            Some("bitmask") => Category::Bitmask,
            Some("struct") => Category::Struct,
            Some("union") => Category::Union,
            Some("funcpointer") => Category::Funcpointer,
            Some(other) => {
                return Err(RegistryError::unsupported(format!(
                    "type category `{other}`"
                )));
            }
        };
        let name = match (&ty.name, &ty.spec) {
            (Some(name), _) => name.clone(),
            (None, TypeSpec::Code(code)) => code
                .markup
                .iter()
                .find_map(|m| match m {
                    TypeCodeMarkup::Name(name) => Some(name.clone()),
                    _ => None,
                })
                .ok_or_else(|| RegistryError::unsupported("a type without a name"))?,
            (None, TypeSpec::Funcpointer(code)) => code.proto.name.clone(),
            (None, _) => return Err(RegistryError::unsupported("a type without a name")),
        };
        if policy::TYPE_BLOCK_LIST.contains(&name.as_str()) {
            return Ok(());
        }
        let id = self.get_type(&name);
        self.init(id, &name, category)?;
        match category {
            Category::Include | Category::Default => {}
            Category::Define => {
                if let TypeSpec::Code(code) = &ty.spec {
                    self.reg.types[id].define = Some(code.code.clone());
                }
            }
            Category::Basetype => {
                if let TypeSpec::Code(code) = &ty.spec {
                    let typedef = first_type_markup(&code.markup).map(str::to_owned);
                    if let Some(typedef) = typedef {
                        let target = self.get_type(&typedef);
                        self.reg.types[id].typedef = Some(target);
                    }
                }
            }
            Category::Handle => {
                if let TypeSpec::Code(code) = &ty.spec {
                    self.reg.types[id].dispatchable =
                        first_type_markup(&code.markup) == Some("VK_DEFINE_HANDLE");
                }
            }
            Category::Enum => self.reg.types[id].enums = Some(Enums::new()),
            Category::Bitmask => {
                let TypeSpec::Code(code) = &ty.spec else {
                    return Err(RegistryError::unsupported(format!(
                        "bitmask {name} without a typedef"
                    )));
                };
                let to = first_type_markup(&code.markup)
                    .unwrap_or_default()
                    .to_owned();
                if to != "VkFlags" && to != "VkFlags64" {
                    return Err(RegistryError::unsupported(format!(
                        "bitmask {name} is a typedef of {to}"
                    )));
                }
                let requires = ty.requires.as_ref().or(ty.bitvalues.as_ref()).cloned();
                let requires = requires.map(|r| self.get_type(&r));
                let typedef = self.get_type(&to);
                let def = &mut self.reg.types[id];
                def.typedef = Some(typedef);
                def.requires = requires;
            }
            Category::Struct | Category::Union => self.parse_members(id, ty)?,
            Category::Funcpointer => self.parse_funcpointer(id, &ty.spec)?,
            Category::Command => unreachable!("commands are parsed from <commands>"),
        }
        Ok(())
    }

    fn parse_members(&mut self, id: TypeId, ty: &XmlType) -> Result<()> {
        let TypeSpec::Members(members) = &ty.spec else {
            return Err(RegistryError::unsupported(format!(
                "struct {} without members",
                self.reg.types[id].name
            )));
        };
        let mut variables = Vec::new();
        for member in members {
            let TypeMember::Definition(member) = member else {
                continue;
            };
            if is_vulkansc(&member.api) {
                continue;
            }
            let static_len = member.markup.iter().find_map(|m| match m {
                TypeMemberMarkup::Enum(text) => Some(text.clone()),
                _ => None,
            });
            let member_name = member
                .markup
                .iter()
                .find_map(|m| match m {
                    TypeMemberMarkup::Name(text) => Some(text.clone()),
                    _ => None,
                })
                .ok_or_else(|| {
                    RegistryError::unsupported(format!(
                        "a member of {} has no <name>",
                        self.reg.types[id].name
                    ))
                })?;
            let var = self.parse_variable(
                &member.code,
                &member_name,
                member.len.as_deref(),
                member.altlen.as_deref(),
                member.optional.as_deref(),
                member.noautovalidity.as_deref(),
                None,
                member.selector.as_deref(),
                member.selection.as_deref(),
                member.values.as_deref(),
                static_len,
            )?;
            variables.push(var);
        }
        // A union's selector type: the type of the sibling the member names.
        let selectors: Vec<(usize, TypeRef)> = variables
            .iter()
            .enumerate()
            .filter_map(|(i, var)| {
                let selector = var.attrs.selector.as_ref()?;
                let sibling = variables.iter().find(|s| &s.name == selector)?;
                Some((i, sibling.ty.clone()))
            })
            .collect();
        for (i, sty) in selectors {
            let base = variables[i].ty.base;
            self.reg.types[base].selector_type = Some(sty);
        }
        let s_type = match variables.first() {
            Some(first) if first.name == "sType" && !first.attrs.values.is_empty() => {
                Some(first.attrs.values[0].clone())
            }
            _ => None,
        };
        let declared: Vec<String> = variables.iter().map(|v| v.name.clone()).collect();
        let variables = sort_struct_members(variables);
        let declared_order = declared
            .iter()
            .map(|name| {
                variables
                    .iter()
                    .position(|v| &v.name == name)
                    .ok_or_else(|| {
                        RegistryError::inconsistent(format!(
                            "member {name} is lost by the member sort"
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let def = &mut self.reg.types[id];
        def.declared_order = declared_order;
        def.variables = variables;
        def.s_type = s_type;
        def.struct_extends = ty
            .structextends
            .as_deref()
            .map(|s| s.split(',').map(str::to_owned).collect())
            .unwrap_or_default();
        def.returned_only = ty.returnedonly.as_deref().is_some_and(|v| v != "false");
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per XML attribute of a member or parameter; a carrier struct would only rename them"
    )]
    fn parse_variable(
        &mut self,
        code: &str,
        name: &str,
        len: Option<&str>,
        altlen: Option<&str>,
        optional: Option<&str>,
        noautovalidity: Option<&str>,
        stride: Option<&str>,
        selector: Option<&str>,
        selection: Option<&str>,
        values: Option<&str>,
        static_len: Option<String>,
    ) -> Result<Variable> {
        let decl = Decl::from_c(code, name)?;
        let ty = self.type_ref(&decl);
        if let (Some(dim), Some(static_len)) = (&decl.decor.dim, &static_len)
            && dim != static_len
        {
            return Err(RegistryError::inconsistent(format!(
                "`{code}`: array size {dim} differs from its <enum> {static_len}"
            )));
        }
        let mut attrs = VarAttrs {
            values: values.map(split_commas).unwrap_or_default(),
            optional: optional.map(split_commas).unwrap_or_default(),
            noautovalidity: noautovalidity == Some("true"),
            stride: stride.map(str::to_owned),
            selector: selector.map(str::to_owned),
            selection: selection.map(split_commas).unwrap_or_default(),
            static_len,
            ..VarAttrs::default()
        };
        if let Some(len) = len {
            let mut lens: Vec<String> = match altlen {
                Some(altlen) => split_commas(altlen),
                None => split_commas(len),
            };
            // The registry has no nested array information for these two.
            if altlen.is_none()
                && ["ppBuildRangeInfos", "ppMaxPrimitiveCounts"].contains(&decl.name.as_str())
            {
                lens.push("pInfos[i].geometryCount".to_owned());
            }
            attrs.lens = lens
                .into_iter()
                .map(|expr| {
                    let name = if expr == "null-terminated" {
                        String::new()
                    } else {
                        extract_len_name(&expr)
                    };
                    Len { expr, name }
                })
                .collect();
        }
        Ok(Variable {
            name: decl.name,
            ty,
            attrs,
        })
    }

    fn parse_funcpointer(&mut self, id: TypeId, spec: &TypeSpec) -> Result<()> {
        let TypeSpec::Funcpointer(code) = spec else {
            return Err(RegistryError::unsupported(
                "a function pointer without a prototype",
            ));
        };
        // The prototype reads `void (VKAPI_PTR *PFN_name)`: the return type is
        // what precedes the parenthesis.
        let return_text = code
            .proto
            .code
            .split('(')
            .next()
            .unwrap_or_default()
            .trim_end();
        let ret = Decl::from_c(&format!("{return_text} ret"), "ret")?;
        let ret_ty = self.type_ref(&ret);
        let mut variables = Vec::new();
        for param in &code.params {
            let decl = Decl::from_c(&param.code, &param.name)?;
            let ty = self.type_ref(&decl);
            variables.push(Variable {
                name: decl.name,
                ty,
                attrs: VarAttrs::default(),
            });
        }
        let void = self.reg.names.get("void").copied();
        let returns_void =
            ret_ty.base == void.unwrap_or(usize::MAX) && ret_ty.decor.ref_quals.is_empty();
        let def = &mut self.reg.types[id];
        def.variables = variables;
        def.ret = (!returns_void).then_some(ret_ty);
        Ok(())
    }

    fn parse_enums(&mut self, enums: &vk_parse::Enums) -> Result<()> {
        let Some(name) = &enums.name else {
            return Ok(());
        };
        match enums.kind.as_deref() {
            Some("constants") => {
                for child in &enums.children {
                    if let EnumsChild::Enum(item) = child {
                        if is_vulkansc(&item.api) {
                            continue;
                        }
                        let (key, value) = enum_key_value(item, None)?;
                        self.reg.constants.insert(key, value);
                    }
                }
                return Ok(());
            }
            Some("enum") | Some("bitmask") => {}
            _ => return Ok(()),
        }
        if policy::TYPE_BLOCK_LIST.contains(&name.as_str()) {
            return Ok(());
        }
        let mut values = Vec::new();
        for child in &enums.children {
            if let EnumsChild::Enum(item) = child {
                if is_vulkansc(&item.api) {
                    continue;
                }
                let (key, value) = enum_key_value(item, None)?;
                if values.iter().any(|(k, _): &(String, String)| *k == key) {
                    return Err(RegistryError::inconsistent(format!(
                        "enum {name} defines {key} twice"
                    )));
                }
                values.push((key, value));
            }
        }
        let id = self.reg.names.get(name).copied().ok_or_else(|| {
            RegistryError::inconsistent(format!("<enums name=\"{name}\"> has no matching <type>"))
        })?;
        let def = &mut self.reg.types[id];
        let target = def.enums.as_mut().ok_or_else(|| {
            RegistryError::inconsistent(format!("{name} has <enums> but is not an enum type"))
        })?;
        if !target.values.is_empty() {
            return Err(RegistryError::inconsistent(format!(
                "{name} has two <enums> blocks"
            )));
        }
        target.bitwidth = enums.bitwidth.unwrap_or(32);
        target.values = values;
        // Extensions that appeared earlier in the file order cannot occur, but
        // keep any that were queued for a not-yet-initialised enum.
        let pending: Vec<_> = std::mem::take(&mut self.pending_enum_values);
        for (extends, key, value) in pending {
            self.apply_enum_extension(&extends, key, value)?;
        }
        Ok(())
    }

    fn apply_enum_extension(&mut self, extends: &str, key: String, value: String) -> Result<()> {
        let id = self.reg.names.get(extends).copied().ok_or_else(|| {
            RegistryError::inconsistent(format!("{key} extends unknown type {extends}"))
        })?;
        let enums = self.reg.types[id].enums.as_mut().ok_or_else(|| {
            RegistryError::inconsistent(format!("{key} extends {extends}, which is not an enum"))
        })?;
        enums.extend_value(key, value)
    }

    fn parse_command(&mut self, command: &Command) -> Result<()> {
        match command {
            Command::Alias { name, alias } => self.parse_alias(Some(name), Some(alias)),
            Command::Definition(command) => {
                if is_vulkansc(&command.api) {
                    return Ok(());
                }
                self.parse_command_definition(command)
            }
            _ => Err(RegistryError::unsupported("an unknown <command> form")),
        }
    }

    fn parse_command_definition(&mut self, command: &CommandDefinition) -> Result<()> {
        let name = command.proto.name.clone();
        let id = self.get_type(&name);
        self.init(id, &name, Category::Command)?;
        let proto = Decl::from_c(&command.proto.code, &command.proto.name)?;
        let ret = self.type_ref(&proto);
        let mut variables = Vec::new();
        for param in &command.params {
            if is_vulkansc(&param.api) {
                continue;
            }
            variables.push(self.parse_param(param)?);
        }
        let returns_void = proto.type_name == "void" && proto.decor.ref_quals.is_empty();
        let def = &mut self.reg.types[id];
        def.variables = variables;
        def.ret = (!returns_void).then_some(ret);
        def.can_device_lost = command
            .errorcodes
            .as_deref()
            .is_some_and(|codes| codes.split(',').any(|c| c == "VK_ERROR_DEVICE_LOST"));
        Ok(())
    }

    fn parse_param(&mut self, param: &CommandParam) -> Result<Variable> {
        // The `<enum>` size of a static array parameter is only in the code.
        // A parameter's XML text has no `<enum>` markup in vk-parse; a size
        // that names a constant is what carries it for members.
        let static_len = Decl::from_c(&param.definition.code, &param.definition.name)?
            .decor
            .dim
            .filter(|dim| !dim.chars().all(|c| c.is_ascii_digit()));
        self.parse_variable(
            &param.definition.code,
            &param.definition.name,
            param.len.as_deref(),
            param.altlen.as_deref(),
            param.optional.as_deref(),
            param.noautovalidity.as_deref(),
            param.stride.as_deref(),
            None,
            None,
            None,
            static_len,
        )
    }

    fn parse_require_items(
        &mut self,
        items: &[InterfaceItem],
        ext_number: Option<i64>,
    ) -> Result<(Vec<TypeId>, Vec<String>)> {
        let mut types = Vec::new();
        let mut names = Vec::new();
        for item in items {
            match item {
                InterfaceItem::Enum(item) => {
                    if is_vulkansc(&item.api) {
                        continue;
                    }
                    let extends = match &item.spec {
                        EnumSpec::Alias { extends, .. }
                        | EnumSpec::Bitpos { extends, .. }
                        | EnumSpec::Value { extends, .. } => extends.clone(),
                        EnumSpec::Offset { extends, .. } => Some(extends.clone()),
                        _ => None,
                    };
                    let Some(extends) = extends else { continue };
                    if policy::TYPE_BLOCK_LIST.contains(&extends.as_str()) {
                        continue;
                    }
                    let (key, value) = enum_key_value(item, ext_number)?;
                    self.apply_enum_extension(&extends, key, value)?;
                }
                InterfaceItem::Type { name, .. } | InterfaceItem::Command { name, .. } => {
                    if policy::TYPE_BLOCK_LIST.contains(&name.as_str()) {
                        continue;
                    }
                    let id = self.reg.names.get(name).copied().ok_or_else(|| {
                        RegistryError::inconsistent(format!("a <require> names unknown {name}"))
                    })?;
                    types.push(id);
                    names.push(name.clone());
                }
                _ => {}
            }
        }
        Ok((types, names))
    }

    fn parse_feature(&mut self, feature: &vk_parse::Feature) -> Result<()> {
        if feature.api == "vulkansc" {
            return Ok(());
        }
        let mut types: Vec<TypeId> = Vec::new();
        for child in &feature.children {
            if let ExtensionChild::Require { api, items, .. } = child {
                if is_vulkansc(api) {
                    continue;
                }
                let (required, _) = self.parse_require_items(items, None)?;
                for ty in required {
                    if !types.contains(&ty) {
                        types.push(ty);
                    }
                }
            }
        }
        self.reg.features.push(Feature {
            name: feature.name.clone(),
            number: feature.number.clone().unwrap_or_default(),
            types,
        });
        Ok(())
    }

    fn parse_extension(&mut self, extension: &vk_parse::Extension) -> Result<()> {
        let number = extension.number.ok_or_else(|| {
            RegistryError::inconsistent(format!("{} has no number", extension.name))
        })?;
        let supported: Vec<String> = extension
            .supported
            .as_deref()
            .map(split_commas)
            .unwrap_or_default();
        let mut ext = Extension {
            name: extension.name.clone(),
            number: u32::try_from(number).map_err(|_| {
                RegistryError::inconsistent(format!("{} has number {number}", extension.name))
            })?,
            supported,
            version: 0,
            types: Vec::new(),
            optional_types: Vec::new(),
        };
        for child in &extension.children {
            let ExtensionChild::Require {
                api,
                depends,
                items,
                ..
            } = child
            else {
                continue;
            };
            if is_vulkansc(api) {
                continue;
            }
            for item in items {
                if let InterfaceItem::Enum(item) = item {
                    if is_vulkansc(&item.api) {
                        continue;
                    }
                    if item.name.ends_with("SPEC_VERSION")
                        && let EnumSpec::Value { value, .. } = &item.spec
                    {
                        ext.version = u32::try_from(parse_int(value)?).map_err(|_| {
                            RegistryError::inconsistent(format!(
                                "{} has a bad spec version",
                                item.name
                            ))
                        })?;
                        break;
                    }
                }
            }
            if !ext.supported.iter().any(|s| s == "vulkan") {
                continue;
            }
            let (types, _) = self.parse_require_items(items, Some(number))?;
            match filter_depends(depends.as_deref()) {
                Some(dependency) => {
                    let slot = match ext
                        .optional_types
                        .iter()
                        .position(|(k, _)| *k == dependency)
                    {
                        Some(i) => i,
                        None => {
                            ext.optional_types.push((dependency, Vec::new()));
                            ext.optional_types.len() - 1
                        }
                    };
                    for ty in types {
                        if !ext.optional_types[slot].1.contains(&ty) {
                            ext.optional_types[slot].1.push(ty);
                        }
                    }
                }
                None => {
                    for ty in types {
                        if !ext.types.contains(&ty) {
                            ext.types.push(ty);
                        }
                    }
                }
            }
        }
        self.reg.extensions.push(ext);
        Ok(())
    }

    /// `VkRegistry._resolve`.
    fn resolve(&mut self, vk_xml_types: &HashSet<TypeId>) -> Result<()> {
        let count = self.reg.types.len();
        let mut extends: Vec<(TypeId, TypeId)> = Vec::new();
        for id in 0..count {
            let ty = &self.reg.types[id];
            if ty.category.is_none() {
                return Err(RegistryError::inconsistent(format!(
                    "type {} is referenced but never defined",
                    ty.name
                )));
            }
            self.reg.types[id].is_private = !vk_xml_types.contains(&id);
            for name in self.reg.types[id].struct_extends.clone() {
                let target = self.reg.names.get(&name).copied().ok_or_else(|| {
                    RegistryError::inconsistent(format!(
                        "{} extends unknown struct {name}",
                        self.reg.types[id].name
                    ))
                })?;
                extends.push((id, target));
            }
        }
        for (id, target) in extends {
            if self.reg.types[target].p_next.contains(&id) {
                return Err(RegistryError::inconsistent(format!(
                    "{} extends {} twice",
                    self.reg.types[id].name, self.reg.types[target].name
                )));
            }
            self.reg.types[target].p_next.push(id);
        }
        // Resolve enum value aliases.
        for ty in &mut self.reg.types {
            let Some(enums) = &mut ty.enums else { continue };
            let snapshot: HashMap<String, String> = enums.values.iter().cloned().collect();
            for (_, value) in &mut enums.values {
                let mut hops = 0;
                while let Some(next) = snapshot.get(value) {
                    *value = next.clone();
                    hops += 1;
                    if hops > 16 {
                        return Err(RegistryError::inconsistent(format!(
                            "enum aliases of {} form a cycle",
                            ty.name
                        )));
                    }
                }
            }
        }
        // `VkCommandTypeEXT` sorted by value.
        let command_type = self.reg.require("VkCommandTypeEXT")?;
        let enums = self.reg.types[command_type]
            .enums
            .as_mut()
            .expect("an enum");
        let mut keyed = Vec::new();
        for (key, value) in std::mem::take(&mut enums.values) {
            keyed.push((parse_int(&value)?, key, value));
        }
        keyed.sort_by_key(|(n, _, _)| *n);
        self.reg.max_command_type_value = keyed
            .last()
            .map(|(n, _, _)| u64::try_from(*n).unwrap_or(0))
            .unwrap_or(0);
        enums.values = keyed.into_iter().map(|(_, k, v)| (k, v)).collect();
        // `VK_HEADER_VERSION_COMPLETE`.
        let version = self.reg.require("VK_HEADER_VERSION")?;
        let complete = self.reg.require("VK_HEADER_VERSION_COMPLETE")?;
        let version_text = self.reg.types[version].define.clone().unwrap_or_default();
        let patch = version_text
            .split_whitespace()
            .last()
            .and_then(|w| w.parse::<u32>().ok())
            .ok_or_else(|| RegistryError::inconsistent("VK_HEADER_VERSION has no value"))?;
        let complete_text = self.reg.types[complete].define.clone().unwrap_or_default();
        let open = complete_text
            .rfind('(')
            .ok_or_else(|| RegistryError::inconsistent("VK_HEADER_VERSION_COMPLETE has no `(`"))?;
        let args = complete_text[open + 1..]
            .trim_end()
            .trim_end_matches(')')
            .replace("VK_HEADER_VERSION", &patch.to_string());
        let parts: Vec<u32> = args
            .split(',')
            .map(|p| p.trim().parse::<u32>())
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| {
                RegistryError::inconsistent("VK_HEADER_VERSION_COMPLETE is not four integers")
            })?;
        self.reg.vk_xml_version = parts.try_into().map_err(|_| {
            RegistryError::inconsistent("VK_HEADER_VERSION_COMPLETE is not four integers")
        })?;
        Ok(())
    }
}

fn first_type_markup(markup: &[TypeCodeMarkup]) -> Option<&str> {
    markup.iter().find_map(|m| match m {
        TypeCodeMarkup::Type(text) => Some(text.as_str()),
        _ => None,
    })
}

fn split_commas(text: &str) -> Vec<String> {
    text.split(',').map(str::to_owned).collect()
}

/// The member path a length expression reads: from its first lowercase letter
/// through identifier characters, `->` and `[i].`.
fn extract_len_name(expr: &str) -> String {
    let bytes = expr.as_bytes();
    let Some(begin) = bytes.iter().position(|b| b.is_ascii_lowercase()) else {
        return String::new();
    };
    let mut end = begin + 1;
    while end < bytes.len() {
        if bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' {
            end += 1;
        } else if expr[end..].starts_with("->") {
            end += 2;
        } else if expr[end..].starts_with("[i].") {
            end += 4;
        } else {
            break;
        }
    }
    expr[begin..end].to_owned()
}

/// `VkType._sort_struct_members`: a dynamic array's length member is moved
/// before the array.
fn sort_struct_members(members: Vec<Variable>) -> Vec<Variable> {
    let mut sorted: Vec<Variable> = Vec::new();
    let mut taken = vec![false; members.len()];
    for (index, var) in members.iter().enumerate() {
        if taken[index] {
            continue;
        }
        for len in &var.attrs.lens {
            let len_name = len.name.split('[').next().unwrap_or("");
            if sorted.iter().any(|s| s.name == len_name) {
                continue;
            }
            if let Some(found) = members
                .iter()
                .enumerate()
                .find(|(_, candidate)| candidate.name == len_name)
            {
                sorted.push(found.1.clone());
                taken[found.0] = true;
            }
        }
        sorted.push(var.clone());
        taken[index] = true;
    }
    sorted
}

/// `VkExtension.filter_depends`.
fn filter_depends(deps: Option<&str>) -> Option<String> {
    let deps = deps?;
    if deps.is_empty() {
        return None;
    }
    if !deps.contains('+') && !deps.contains(',') {
        if deps.starts_with("VK_VERSION") || !deps.starts_with("VK_") {
            return None;
        }
        return Some(deps.to_owned());
    }
    let mut or_list = Vec::new();
    for dep in top_level_split(deps, ',') {
        if !dep.contains('+') && dep.starts_with("VK_VERSION") {
            continue;
        }
        let mut and_list = Vec::new();
        for and_dep in top_level_split(&dep, '+') {
            // `removeprefix` and `removesuffix` are independent in the Python.
            let trimmed = and_dep.strip_prefix('(').unwrap_or(&and_dep);
            let trimmed = trimmed.strip_suffix(')').unwrap_or(trimmed);
            if let Some(sub) = filter_depends(Some(trimmed)) {
                and_list.push(sub);
            }
        }
        if and_list.is_empty() {
            return None;
        }
        let mut or_dep = and_list.join("+");
        if and_list.len() > 1 {
            or_dep = format!("({or_dep})");
        }
        or_list.push(or_dep);
    }
    let mut filtered = or_list.join(",");
    if or_list.len() > 1 {
        filtered = format!("({filtered})");
    }
    Some(filtered)
}

/// `VkExtension.top_level_split`.
pub fn top_level_split(text: &str, delimiter: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    for c in text.chars() {
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
        }
        if c == delimiter && depth == 0 {
            parts.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        parts.push(current.trim().to_owned());
    }
    parts
}

/// `VkEnums._parse_enum`: the name and the textual value.
fn enum_key_value(item: &XmlEnum, ext_number: Option<i64>) -> Result<(String, String)> {
    let key = item.name.clone();
    let value = match &item.spec {
        EnumSpec::Alias { alias, .. } => alias.clone(),
        EnumSpec::Value { value, .. } => value.clone(),
        EnumSpec::Bitpos { bitpos, .. } => format!("0x{:08x}", 1u128 << bitpos),
        EnumSpec::Offset {
            offset,
            extnumber,
            dir,
            ..
        } => {
            let number = extnumber.or(ext_number).ok_or_else(|| {
                RegistryError::inconsistent(format!("{key} has an offset and no extension number"))
            })?;
            let value = 1_000_000_000 + (number - 1) * 1000 + offset;
            // vk-parse's `dir` is whether the offset is positive.
            if *dir {
                value.to_string()
            } else {
                format!("-{value}")
            }
        }
        _ => {
            return Err(RegistryError::unsupported(format!(
                "enum {key} without a value"
            )));
        }
    };
    Ok((key, value))
}
