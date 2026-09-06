//! A read-only view of one core wasm module: its sections as byte spans,
//! the index spaces the rewrites need, and every branch site in its code.

use std::ops::Range;

use wasmparser::{ExternalKind, FuncType, MemoryType, Operator, Parser, Payload};

use crate::{BranchKind, Error, FD_WRITE, IMPORT_MODULE, PROC_EXIT, wasm};

pub const TYPE_SECTION: u8 = 1;
pub const IMPORT_SECTION: u8 = 2;
pub const FUNCTION_SECTION: u8 = 3;
pub const MEMORY_SECTION: u8 = 5;
pub const GLOBAL_SECTION: u8 = 6;
pub const EXPORT_SECTION: u8 = 7;
pub const CODE_SECTION: u8 = 10;

/// One section of the module, as the bytes it occupies.
#[derive(Debug, Clone)]
pub struct SectionSpan {
    pub id: u8,
    /// Offset of the section id byte.
    pub header_start: usize,
    /// The section's contents, count prefix included.
    pub content: Range<usize>,
    /// Number of entries, for the vector-shaped sections.
    pub count: u32,
}

impl SectionSpan {
    /// The whole section, header included.
    pub fn full(&self) -> Range<usize> {
        self.header_start..self.content.end
    }
}

/// One `if` or `br_if` in a function body.
#[derive(Debug, Clone, Copy)]
pub struct BranchSite {
    /// Index in the module's function index space, imports included.
    pub func: u32,
    /// Offset of the branch opcode from the start of the function body,
    /// which is where the branch-hinting proposal counts from and what
    /// `FuncEnvironment::take_branch_hint` subtracts.
    pub offset: u32,
    /// Absolute offset of the branch opcode in the module.
    pub absolute: usize,
    pub kind: BranchKind,
}

/// One function body in the code section.
#[derive(Debug, Clone)]
pub struct Body {
    /// Index in the module's function index space, imports included.
    pub func: u32,
    /// The body's contents, starting at the locals declaration; the size
    /// prefix is not part of it. This is what a hint offset counts from.
    pub content: Range<usize>,
    /// The whole code section entry, size prefix included.
    pub entry: Range<usize>,
    /// Offset of the first instruction.
    pub instructions_start: usize,
    /// Number of parameters the function takes.
    pub params: u32,
    /// Number of local slots the body declares, params excluded.
    pub locals: u32,
    /// Number of local declaration groups.
    pub local_groups: u32,
    pub branches: Vec<BranchSite>,
    /// Byte ranges of the `call` instructions targeting the module's
    /// `proc_exit` import, which the instrumenter reroutes so a program
    /// that exits non-locally still dumps its counters.
    pub proc_exit_calls: Vec<Range<usize>>,
}

/// One export entry.
#[derive(Debug, Clone)]
pub struct ExportEntry {
    pub name: String,
    pub kind: ExternalKind,
    pub index: u32,
    /// The entry's own bytes in the export section.
    pub entry: Range<usize>,
    /// Offset of the entry's index, the one field a rewrite changes.
    pub index_start: usize,
}

/// A parsed core module.
#[derive(Debug)]
pub struct CoreModule<'a> {
    pub bytes: &'a [u8],
    pub sections: Vec<SectionSpan>,
    /// Function types by type index; `None` for a non-function type.
    pub types: Vec<Option<FuncType>>,
    /// Type index of each imported function, in index order.
    pub imported_func_types: Vec<u32>,
    /// Type index of each defined function, in index order.
    pub defined_func_types: Vec<u32>,
    pub memory: Option<MemoryType>,
    pub memory_imported: bool,
    pub globals: u32,
    pub exports: Vec<ExportEntry>,
    pub bodies: Vec<Body>,
    /// Function index of `wasi_snapshot_preview1.fd_write`, if imported.
    pub fd_write: Option<u32>,
    /// Function index of `wasi_snapshot_preview1.proc_exit`, if imported.
    pub proc_exit: Option<u32>,
}

impl<'a> CoreModule<'a> {
    /// Parses `bytes` as a core module.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, Error> {
        let mut module = CoreModule {
            bytes,
            sections: Vec::new(),
            types: Vec::new(),
            imported_func_types: Vec::new(),
            defined_func_types: Vec::new(),
            memory: None,
            memory_imported: false,
            globals: 0,
            exports: Vec::new(),
            bodies: Vec::new(),
            fd_write: None,
            proc_exit: None,
        };
        let mut memories = 0usize;

        for payload in Parser::new(0).parse_all(bytes) {
            let payload = payload.map_err(Error::parse("core module"))?;
            if let Some((id, content)) = payload.as_section() {
                module.sections.push(SectionSpan {
                    id,
                    header_start: header_start(bytes, id, &content),
                    content,
                    count: 0,
                });
            }
            match payload {
                Payload::TypeSection(reader) => {
                    let count = reader.count();
                    for group in reader {
                        let group = group.map_err(Error::parse("type section"))?;
                        for ty in group.into_types() {
                            module.types.push(match ty.composite_type.inner {
                                wasmparser::CompositeInnerType::Func(func) => Some(func),
                                _ => None,
                            });
                        }
                    }
                    module.set_count(TYPE_SECTION, count);
                }
                Payload::ImportSection(reader) => {
                    let count = reader.count();
                    for import in reader.into_imports() {
                        let import = import.map_err(Error::parse("import section"))?;
                        match import.ty {
                            wasmparser::TypeRef::Func(type_index)
                            | wasmparser::TypeRef::FuncExact(type_index) => {
                                let func = u32::try_from(module.imported_func_types.len())
                                    .expect("function index space fits in a u32");
                                if import.module == IMPORT_MODULE {
                                    match import.name {
                                        FD_WRITE => module.fd_write = Some(func),
                                        PROC_EXIT => module.proc_exit = Some(func),
                                        _ => {}
                                    }
                                }
                                module.imported_func_types.push(type_index);
                            }
                            wasmparser::TypeRef::Memory(_) => {
                                module.memory_imported = true;
                                memories += 1;
                            }
                            wasmparser::TypeRef::Global(_) => module.globals += 1,
                            _ => {}
                        }
                    }
                    module.set_count(IMPORT_SECTION, count);
                }
                Payload::FunctionSection(reader) => {
                    let count = reader.count();
                    for type_index in reader {
                        module
                            .defined_func_types
                            .push(type_index.map_err(Error::parse("function section"))?);
                    }
                    module.set_count(FUNCTION_SECTION, count);
                }
                Payload::MemorySection(reader) => {
                    let count = reader.count();
                    for memory in reader {
                        let memory = memory.map_err(Error::parse("memory section"))?;
                        module.memory = Some(memory);
                        memories += 1;
                    }
                    module.set_count(MEMORY_SECTION, count);
                }
                Payload::GlobalSection(reader) => {
                    let count = reader.count();
                    module.globals += count;
                    module.set_count(GLOBAL_SECTION, count);
                }
                Payload::ExportSection(reader) => {
                    let count = reader.count();
                    let end = reader.range().end;
                    let mut starts = Vec::new();
                    for entry in reader.into_iter_with_offsets() {
                        let (start, export) = entry.map_err(Error::parse("export section"))?;
                        starts.push(start);
                        let index_start = start
                            + wasm::leb_u32(
                                u32::try_from(export.name.len())
                                    .expect("an export name is shorter than 4 GiB"),
                            )
                            .len()
                            + export.name.len()
                            + 1;
                        module.exports.push(ExportEntry {
                            name: export.name.to_owned(),
                            kind: export.kind,
                            index: export.index,
                            entry: start..start,
                            index_start,
                        });
                    }
                    let base = module.exports.len() - starts.len();
                    for position in 0..starts.len() {
                        let entry_end = starts.get(position + 1).copied().unwrap_or(end);
                        module.exports[base + position].entry = starts[position]..entry_end;
                    }
                    module.set_count(EXPORT_SECTION, count);
                }
                Payload::CodeSectionStart { count, .. } => {
                    module.set_count(CODE_SECTION, count);
                }
                Payload::CodeSectionEntry(body) => {
                    let func =
                        u32::try_from(module.imported_func_types.len() + module.bodies.len())
                            .expect("function index space fits in a u32");
                    let proc_exit = module.proc_exit;
                    let params = u32::try_from(module.func_type(func)?.params().len())
                        .expect("a function takes fewer than 4G parameters");
                    module
                        .bodies
                        .push(read_body(func, params, &body, proc_exit)?);
                }
                _ => {}
            }
        }

        if memories > 1 {
            return Err(Error::MultipleMemories);
        }
        Ok(module)
    }

    fn set_count(&mut self, id: u8, count: u32) {
        if let Some(section) = self.sections.iter_mut().rev().find(|s| s.id == id) {
            section.count = count;
        }
    }

    /// The section with `id`, if the module has one.
    pub fn section(&self, id: u8) -> Option<&SectionSpan> {
        self.sections.iter().find(|section| section.id == id)
    }

    /// Total functions, imported and defined.
    pub fn func_count(&self) -> u32 {
        u32::try_from(self.imported_func_types.len() + self.defined_func_types.len())
            .expect("function index space fits in a u32")
    }

    /// The type of function `func`.
    pub fn func_type(&self, func: u32) -> Result<&FuncType, Error> {
        let imported = u32::try_from(self.imported_func_types.len())
            .expect("function index space fits in a u32");
        let type_index = if func < imported {
            self.imported_func_types[func as usize]
        } else {
            *self
                .defined_func_types
                .get((func - imported) as usize)
                .ok_or(Error::MissingBody { func })?
        };
        self.types
            .get(type_index as usize)
            .and_then(Option::as_ref)
            .ok_or(Error::BadTypeIndex(type_index))
    }

    /// The body of function `func`.
    pub fn body(&self, func: u32) -> Result<&Body, Error> {
        self.bodies
            .iter()
            .find(|body| body.func == func)
            .ok_or(Error::MissingBody { func })
    }

    /// Every branch site in the module, in ascending function and offset
    /// order — the order the hint section requires.
    pub fn branch_sites(&self) -> impl Iterator<Item = &BranchSite> {
        self.bodies.iter().flat_map(|body| body.branches.iter())
    }

    /// The sha256 of the code section's contents, the fingerprint a
    /// profile is keyed by: it is exactly the bytes a recorded offset
    /// indexes into, and it ignores the custom sections a rebuild may
    /// reorder or drop.
    pub fn code_fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        if let Some(code) = self.section(CODE_SECTION) {
            hasher.update(&self.bytes[code.content.clone()]);
        }
        format!("{:x}", hasher.finalize())
    }
}

fn header_start(bytes: &[u8], id: u8, content: &Range<usize>) -> usize {
    let size = u32::try_from(content.len()).expect("a wasm section length fits in a u32");
    let start = content.start - 1 - wasm::leb_u32(size).len();
    assert_eq!(
        bytes[start], id,
        "section header for id {id} is not where its length says it is"
    );
    start
}

fn read_body(
    func: u32,
    params: u32,
    body: &wasmparser::FunctionBody<'_>,
    proc_exit: Option<u32>,
) -> Result<Body, Error> {
    let content = body.range();
    let mut locals_reader = body
        .get_locals_reader()
        .map_err(Error::parse("function locals"))?;
    let local_groups = locals_reader.get_count();
    let mut locals = 0u32;
    for _ in 0..local_groups {
        let (count, _) = locals_reader
            .read()
            .map_err(Error::parse("function locals"))?;
        locals += count;
    }

    let mut operators = body
        .get_operators_reader()
        .map_err(Error::parse("function body"))?;
    let instructions_start = operators.original_position();
    let mut branches = Vec::new();
    let mut proc_exit_calls = Vec::new();
    while !operators.eof() {
        let (operator, absolute) = operators
            .read_with_offset()
            .map_err(Error::parse("function body"))?;
        let kind = match operator {
            Operator::If { .. } => BranchKind::If,
            Operator::BrIf { .. } => BranchKind::BrIf,
            Operator::Call { function_index } if Some(function_index) == proc_exit => {
                proc_exit_calls.push(absolute..operators.original_position());
                continue;
            }
            _ => continue,
        };
        branches.push(BranchSite {
            func,
            offset: u32::try_from(absolute - content.start)
                .expect("a function body is shorter than 4 GiB"),
            absolute,
            kind,
        });
    }

    let entry_start = content.start
        - wasm::leb_u32(u32::try_from(content.len()).expect("a function body fits in a u32")).len();
    Ok(Body {
        func,
        entry: entry_start..content.end,
        content,
        instructions_start,
        params,
        locals,
        local_groups,
        branches,
        proc_exit_calls,
    })
}
