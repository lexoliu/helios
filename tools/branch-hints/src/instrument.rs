//! Rewriting a module so a real run records which way each branch went.
//!
//! Every `if` and `br_if` gets a taken/not-taken counter pair in a region
//! reserved at the top of the module's own linear memory, and the program's
//! two exits — `_start` returning and `proc_exit` being called — dump those
//! counters to stdout. Nothing else changes: no function is renumbered, no
//! section this crate has no opinion about is re-encoded, and the offsets
//! the site map records are the *original* module's, because that is the
//! module the hints are eventually written into.
//!
//! Why the counters live inside memory 0 rather than in a second memory or
//! in globals: the guest reaches `fd_write` through memory 0 (a wasi host
//! function reads iovecs from the default memory and nowhere else), the
//! kernel's pooling allocator gives an instance one memory, and a global
//! per site would put hundreds of kilobytes into the instance's vmctx. The
//! region is carved out by raising the memory's *minimum* size: wasi-libc's
//! allocator takes every byte it hands out from `memory.grow`, which starts
//! above the minimum, so the reserved pages are never allocated to the
//! program.

use wasm_encoder::{
    BlockType, ConstExpr, Encode, Function, GlobalType, Instruction, MemArg, MemoryType, ValType,
};

use crate::{
    BEGIN_MARKER, BUFFER_BYTES, COUNTER_BYTES, END_MARKER, ENTRY_EXPORT, Error, FD_WRITE,
    SCRATCH_BYTES, component,
    module::{
        Body, CODE_SECTION, CoreModule, EXPORT_SECTION, FUNCTION_SECTION, GLOBAL_SECTION,
        MEMORY_SECTION, TYPE_SECTION,
    },
    profile::{Site, SiteMap},
    wasm::{self, Splice},
};

const WASM_PAGE: u32 = 64 * 1024;

/// Headroom kept in the output buffer so one record and the trailing
/// marker always fit between two flush checks.
const FLUSH_HEADROOM: u32 = 128;

/// A module rewritten to count its own branches, and the map from counter
/// index back to the original module's branch sites.
pub struct Instrumented {
    pub wasm: Vec<u8>,
    pub sites: SiteMap,
    pub reserved_pages: u32,
    /// Where in the input the rewritten core module came from.
    pub location: component::Location,
}

/// Where the instrumentation's own state lives in the guest's memory.
struct Layout {
    counters: u32,
    scratch: u32,
    buffer: u32,
    flush_limit: u32,
    reserved_pages: u32,
}

impl Layout {
    fn new(minimum_pages: u64, sites: u32) -> Self {
        let base = u32::try_from(minimum_pages * u64::from(WASM_PAGE))
            .expect("an instrumentable module's memory starts below 4 GiB");
        let counters = base;
        let scratch = counters + sites * COUNTER_BYTES;
        let buffer = scratch + SCRATCH_BYTES;
        let end = buffer + BUFFER_BYTES;
        Layout {
            counters,
            scratch,
            buffer,
            flush_limit: end - FLUSH_HEADROOM,
            reserved_pages: (end - base).div_ceil(WASM_PAGE),
        }
    }
}

/// Function indices of the code the instrumenter appends.
struct Added {
    count: u32,
    hex64: u32,
    flush: u32,
    dump: u32,
    start: u32,
    exit: Option<u32>,
}

/// Rewrites `input` — a core module or a component carrying one — to
/// record its own branch counts.
pub fn instrument(input: &[u8], source: &str) -> Result<Instrumented, Error> {
    let location = component::locate(input)?;
    let module = component::core_module(input, &location)?;

    if module.memory_imported {
        return Err(Error::ImportedMemory);
    }
    let memory = module.memory.ok_or(Error::NoMemory)?;
    assert!(
        !memory.memory64 && memory.page_size_log2.is_none_or(|log2| log2 == 16),
        "instrumentation assumes a 32-bit memory with 64 KiB pages"
    );
    let fd_write = module
        .fd_write
        .ok_or(Error::MissingImport { name: FD_WRITE })?;
    let entry = entry_function(&module)?;

    let sites: Vec<Site> = module
        .branch_sites()
        .map(|site| Site {
            func: site.func,
            offset: site.offset,
        })
        .collect();
    let site_count = u32::try_from(sites.len()).expect("a module holds fewer than 4G branches");
    let layout = Layout::new(memory.initial, site_count);

    let base = module.func_count();
    let added = Added {
        count: base,
        hex64: base + 1,
        flush: base + 2,
        dump: base + 3,
        start: base + 4,
        exit: module.proc_exit.map(|_| base + 5),
    };
    let type_base = u32::try_from(module.types.len()).expect("a module holds fewer than 4G types");
    let global = module.globals;

    let wasm = rewrite(
        &module, &layout, &added, type_base, global, entry, fd_write, memory,
    )?;

    Ok(Instrumented {
        wasm: component::splice(input, &location, &wasm),
        sites: SiteMap {
            version: 1,
            source: source.to_owned(),
            core_module_index: location.core_module_index,
            code_fingerprint: module.code_fingerprint(),
            sites,
        },
        reserved_pages: layout.reserved_pages,
        location,
    })
}

/// The function `_start` exports, which the instrumenter wraps.
fn entry_function(module: &CoreModule<'_>) -> Result<u32, Error> {
    let export = module
        .exports
        .iter()
        .find(|export| export.name == ENTRY_EXPORT && export.kind == wasmparser::ExternalKind::Func)
        .ok_or(Error::MissingEntry)?;
    let ty = module.func_type(export.index)?;
    if !ty.params().is_empty() || !ty.results().is_empty() {
        return Err(Error::EntryNotNullary { func: export.index });
    }
    Ok(export.index)
}

#[expect(
    clippy::too_many_arguments,
    reason = "these are the module facts the rewrite needs; bundling them into a struct \
              would only move the argument list"
)]
fn rewrite(
    module: &CoreModule<'_>,
    layout: &Layout,
    added: &Added,
    type_base: u32,
    global: u32,
    entry: u32,
    fd_write: u32,
    memory: wasmparser::MemoryType,
) -> Result<Vec<u8>, Error> {
    let bytes = module.bytes;

    let extra_types = [
        wasm::func_type_entry(&[ValType::I32, ValType::I32], &[]),
        wasm::func_type_entry(&[ValType::I32, ValType::I64], &[ValType::I32]),
        wasm::func_type_entry(&[ValType::I32], &[ValType::I32]),
        wasm::func_type_entry(&[], &[]),
        wasm::func_type_entry(&[ValType::I32], &[]),
    ]
    .concat();
    // In the order the bodies are appended: count, hex64, flush, dump,
    // start and, when the module has a `proc_exit` import, exit.
    let mut type_indices = vec![
        type_base,
        type_base + 1,
        type_base + 2,
        type_base + 3,
        type_base + 3,
    ];
    if added.exit.is_some() {
        type_indices.push(type_base + 4);
    }
    let added_funcs = u32::try_from(type_indices.len()).expect("at most six added functions");
    let extra_funcs: Vec<u8> = type_indices
        .iter()
        .flat_map(|index| wasm::leb_u32(*index))
        .collect();

    let mut extra_global = Vec::new();
    GlobalType {
        val_type: ValType::I32,
        mutable: true,
        shared: false,
    }
    .encode(&mut extra_global);
    ConstExpr::i32_const(0).encode(&mut extra_global);

    let mut sections: Vec<(u8, Vec<u8>)> = Vec::with_capacity(module.sections.len() + 1);
    for section in &module.sections {
        let content = &bytes[section.content.clone()];
        let rebuilt = match section.id {
            TYPE_SECTION => wasm::append_to_vector_section(content, section.count, 5, &extra_types),
            FUNCTION_SECTION => {
                wasm::append_to_vector_section(content, section.count, added_funcs, &extra_funcs)
            }
            MEMORY_SECTION => memory_section(memory, layout.reserved_pages),
            GLOBAL_SECTION => {
                wasm::append_to_vector_section(content, section.count, 1, &extra_global)
            }
            EXPORT_SECTION => export_section(module, added.start),
            CODE_SECTION => {
                code_section(module, layout, added, global, entry, fd_write, added_funcs)?
            }
            _ => content.to_vec(),
        };
        sections.push((section.id, rebuilt));
    }

    if module.section(GLOBAL_SECTION).is_none() {
        // A module with no globals of its own still needs the guard the
        // dump reads. The binary format fixes where a global section goes.
        let mut content = wasm::leb_u32(1);
        content.extend_from_slice(&extra_global);
        let at = sections
            .iter()
            .position(|(id, _)| *id != 0 && *id > GLOBAL_SECTION)
            .unwrap_or(sections.len());
        sections.insert(at, (GLOBAL_SECTION, content));
    }

    let mut out = bytes[..8].to_vec();
    for (id, content) in sections {
        out.extend(wasm::section(id, &content));
    }
    Ok(out)
}

fn memory_section(memory: wasmparser::MemoryType, reserved_pages: u32) -> Vec<u8> {
    let minimum = memory.initial + u64::from(reserved_pages);
    let mut content = wasm::leb_u32(1);
    MemoryType {
        minimum,
        maximum: memory.maximum.map(|maximum| maximum.max(minimum)),
        memory64: memory.memory64,
        shared: memory.shared,
        page_size_log2: memory.page_size_log2,
    }
    .encode(&mut content);
    content
}

/// Rebuilds the export section with `_start` pointing at the wrapper.
///
/// Every other entry keeps its own bytes, so an export form this crate has
/// no opinion about survives untouched.
fn export_section(module: &CoreModule<'_>, wrapper: u32) -> Vec<u8> {
    let bytes = module.bytes;
    let mut content = wasm::leb_u32(
        u32::try_from(module.exports.len()).expect("a module holds fewer than 4G exports"),
    );
    for export in &module.exports {
        if export.name == ENTRY_EXPORT && export.kind == wasmparser::ExternalKind::Func {
            content.extend_from_slice(&bytes[export.entry.start..export.index_start]);
            content.extend(wasm::leb_u32(wrapper));
        } else {
            content.extend_from_slice(&bytes[export.entry.clone()]);
        }
    }
    content
}

fn code_section(
    module: &CoreModule<'_>,
    layout: &Layout,
    added: &Added,
    global: u32,
    entry: u32,
    fd_write: u32,
    added_funcs: u32,
) -> Result<Vec<u8>, Error> {
    let bytes = module.bytes;
    let mut content = wasm::leb_u32(
        u32::try_from(module.bodies.len()).expect("a module holds fewer than 4G functions")
            + added_funcs,
    );

    let mut site = 0u32;
    for body in &module.bodies {
        content.extend(wasm::sized_body(&instrumented_body(
            bytes, body, &mut site, added,
        )));
    }

    let site_count = site;
    for function in [
        count_function(layout),
        hex64_function(),
        flush_function(layout, fd_write),
        dump_function(layout, added, global, site_count),
        start_function(entry, added.dump),
    ] {
        content.extend(wasm::sized_body(&function.into_raw_body()));
    }
    if let Some(proc_exit) = module.proc_exit {
        content.extend(wasm::sized_body(
            &exit_function(added.dump, proc_exit).into_raw_body(),
        ));
    }
    Ok(content)
}

/// One function body with its branch counters and its `proc_exit` calls
/// rerouted, and one scratch local appended for the condition.
fn instrumented_body(bytes: &[u8], body: &Body, site: &mut u32, added: &Added) -> Vec<u8> {
    let scratch = params_and_locals(body);
    let mut splices = Vec::with_capacity(body.branches.len() + body.proc_exit_calls.len());
    for branch in &body.branches {
        let mut probe = Vec::new();
        for instruction in [
            Instruction::LocalSet(scratch),
            Instruction::I32Const(as_i32(*site)),
            Instruction::LocalGet(scratch),
            Instruction::Call(added.count),
            Instruction::LocalGet(scratch),
        ] {
            instruction.encode(&mut probe);
        }
        let at = branch.absolute - body.instructions_start;
        splices.push(Splice {
            range: at..at,
            bytes: probe,
        });
        *site += 1;
    }
    if let Some(exit) = added.exit {
        for call in &body.proc_exit_calls {
            let mut rerouted = Vec::new();
            Instruction::Call(exit).encode(&mut rerouted);
            splices.push(Splice {
                range: call.start - body.instructions_start..call.end - body.instructions_start,
                bytes: rerouted,
            });
        }
    }

    let mut out = wasm::leb_u32(body.local_groups + 1);
    let groups_start = wasm::read_leb_u32(bytes, body.content.start)
        .expect("a function body starts with a decodable local count")
        .1;
    out.extend_from_slice(&bytes[groups_start..body.instructions_start]);
    out.extend(wasm::leb_u32(1));
    ValType::I32.encode(&mut out);
    out.extend(wasm::apply_splices(
        &bytes[body.instructions_start..body.content.end],
        splices,
    ));
    out
}

fn params_and_locals(body: &Body) -> u32 {
    body.params + body.locals
}

fn as_i32(value: u32) -> i32 {
    i32::try_from(value).expect("wasm addresses and indices this crate emits stay below 2 GiB")
}

fn mem_i64(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 3,
        memory_index: 0,
    }
}

fn mem_i32(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 2,
        memory_index: 0,
    }
}

fn mem_i8(offset: u64) -> MemArg {
    MemArg {
        offset,
        align: 0,
        memory_index: 0,
    }
}

/// `(idx: i32, condition: i32) -> ()`: bumps one of the site's two
/// counters, choosing between them with arithmetic rather than a branch so
/// the probe does not itself change the branch behaviour being measured.
fn count_function(layout: &Layout) -> Function {
    let mut function = Function::new([(1, ValType::I32)]);
    for instruction in [
        Instruction::LocalGet(0),
        Instruction::I32Const(as_i32(COUNTER_BYTES)),
        Instruction::I32Mul,
        Instruction::I32Const(as_i32(layout.counters)),
        Instruction::I32Add,
        Instruction::LocalGet(1),
        Instruction::I32Eqz,
        Instruction::I32Const(8),
        Instruction::I32Mul,
        Instruction::I32Add,
        Instruction::LocalTee(2),
        Instruction::LocalGet(2),
        Instruction::I64Load(mem_i64(0)),
        Instruction::I64Const(1),
        Instruction::I64Add,
        Instruction::I64Store(mem_i64(0)),
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}

/// `(p: i32, value: i64) -> i32`: writes `value` as sixteen lowercase hex
/// digits at `p` and returns the next free byte.
fn hex64_function() -> Function {
    let mut function = Function::new([(2, ValType::I32)]);
    for instruction in [
        Instruction::I32Const(64),
        Instruction::LocalSet(2),
        Instruction::Loop(BlockType::Empty),
        Instruction::LocalGet(2),
        Instruction::I32Const(4),
        Instruction::I32Sub,
        Instruction::LocalSet(2),
        Instruction::LocalGet(0),
        Instruction::LocalGet(1),
        Instruction::LocalGet(2),
        Instruction::I64ExtendI32U,
        Instruction::I64ShrU,
        Instruction::I32WrapI64,
        Instruction::I32Const(0xf),
        Instruction::I32And,
        Instruction::LocalTee(3),
        Instruction::I32Const(10),
        Instruction::I32LtU,
        Instruction::If(BlockType::Result(ValType::I32)),
        Instruction::LocalGet(3),
        Instruction::I32Const(b'0'.into()),
        Instruction::I32Add,
        Instruction::Else,
        Instruction::LocalGet(3),
        Instruction::I32Const(i32::from(b'a') - 10),
        Instruction::I32Add,
        Instruction::End,
        Instruction::I32Store8(mem_i8(0)),
        Instruction::LocalGet(0),
        Instruction::I32Const(1),
        Instruction::I32Add,
        Instruction::LocalSet(0),
        Instruction::LocalGet(2),
        Instruction::BrIf(0),
        Instruction::End,
        Instruction::LocalGet(0),
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}

/// `(p: i32) -> i32`: writes everything between the buffer's start and `p`
/// to stdout and returns the buffer's start. A short write is retried; an
/// error ends the dump, because nothing downstream can use a profile with
/// a hole in it and the parser rejects one without its end marker.
fn flush_function(layout: &Layout, fd_write: u32) -> Function {
    let mut function = Function::new([(2, ValType::I32)]);
    for instruction in [
        Instruction::I32Const(as_i32(layout.buffer)),
        Instruction::LocalSet(1),
        Instruction::Block(BlockType::Empty),
        Instruction::Loop(BlockType::Empty),
        Instruction::LocalGet(1),
        Instruction::LocalGet(0),
        Instruction::I32GeU,
        Instruction::BrIf(1),
        Instruction::I32Const(as_i32(layout.scratch)),
        Instruction::LocalGet(1),
        Instruction::I32Store(mem_i32(0)),
        Instruction::I32Const(as_i32(layout.scratch)),
        Instruction::LocalGet(0),
        Instruction::LocalGet(1),
        Instruction::I32Sub,
        Instruction::I32Store(mem_i32(4)),
        Instruction::I32Const(1),
        Instruction::I32Const(as_i32(layout.scratch)),
        Instruction::I32Const(1),
        Instruction::I32Const(as_i32(layout.scratch + 8)),
        Instruction::Call(fd_write),
        Instruction::BrIf(1),
        Instruction::I32Const(as_i32(layout.scratch)),
        Instruction::I32Load(mem_i32(8)),
        Instruction::LocalTee(2),
        Instruction::I32Eqz,
        Instruction::BrIf(1),
        Instruction::LocalGet(1),
        Instruction::LocalGet(2),
        Instruction::I32Add,
        Instruction::LocalSet(1),
        Instruction::Br(0),
        Instruction::End,
        Instruction::End,
        Instruction::I32Const(as_i32(layout.buffer)),
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}

/// `() -> ()`: writes every site whose counters moved, once.
fn dump_function(layout: &Layout, added: &Added, global: u32, sites: u32) -> Function {
    let mut function = Function::new([(3, ValType::I32), (2, ValType::I64)]);
    let (index, pointer, address) = (0, 1, 2);
    let (taken, not_taken) = (3, 4);

    for instruction in [
        Instruction::GlobalGet(global),
        Instruction::If(BlockType::Empty),
        Instruction::Return,
        Instruction::End,
        Instruction::I32Const(1),
        Instruction::GlobalSet(global),
        Instruction::I32Const(as_i32(layout.buffer)),
        Instruction::LocalSet(pointer),
    ] {
        function.instruction(&instruction);
    }
    write_literal(&mut function, pointer, &begin_marker());

    for instruction in [
        Instruction::I32Const(0),
        Instruction::LocalSet(index),
        Instruction::Block(BlockType::Empty),
        Instruction::Loop(BlockType::Empty),
        Instruction::LocalGet(index),
        Instruction::I32Const(as_i32(sites)),
        Instruction::I32GeU,
        Instruction::BrIf(1),
        Instruction::LocalGet(index),
        Instruction::I32Const(as_i32(COUNTER_BYTES)),
        Instruction::I32Mul,
        Instruction::I32Const(as_i32(layout.counters)),
        Instruction::I32Add,
        Instruction::LocalTee(address),
        Instruction::I64Load(mem_i64(0)),
        Instruction::LocalSet(taken),
        Instruction::LocalGet(address),
        Instruction::I64Load(mem_i64(8)),
        Instruction::LocalSet(not_taken),
        Instruction::LocalGet(taken),
        Instruction::LocalGet(not_taken),
        Instruction::I64Or,
        Instruction::I64Eqz,
        Instruction::I32Eqz,
        Instruction::If(BlockType::Empty),
        Instruction::LocalGet(pointer),
        Instruction::LocalGet(index),
        Instruction::I64ExtendI32U,
        Instruction::Call(added.hex64),
        Instruction::LocalGet(taken),
        Instruction::Call(added.hex64),
        Instruction::LocalGet(not_taken),
        Instruction::Call(added.hex64),
        Instruction::LocalTee(pointer),
        Instruction::I32Const(b'\n'.into()),
        Instruction::I32Store8(mem_i8(0)),
        Instruction::LocalGet(pointer),
        Instruction::I32Const(1),
        Instruction::I32Add,
        Instruction::LocalSet(pointer),
        Instruction::LocalGet(pointer),
        Instruction::I32Const(as_i32(layout.flush_limit)),
        Instruction::I32GeU,
        Instruction::If(BlockType::Empty),
        Instruction::LocalGet(pointer),
        Instruction::Call(added.flush),
        Instruction::LocalSet(pointer),
        Instruction::End,
        Instruction::End,
        Instruction::LocalGet(index),
        Instruction::I32Const(1),
        Instruction::I32Add,
        Instruction::LocalSet(index),
        Instruction::Br(0),
        Instruction::End,
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }

    write_literal(&mut function, pointer, &end_marker());
    for instruction in [
        Instruction::LocalGet(pointer),
        Instruction::Call(added.flush),
        Instruction::Drop,
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}

fn start_function(entry: u32, dump: u32) -> Function {
    let mut function = Function::new([]);
    for instruction in [
        Instruction::Call(entry),
        Instruction::Call(dump),
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}

fn exit_function(dump: u32, proc_exit: u32) -> Function {
    let mut function = Function::new([]);
    for instruction in [
        Instruction::Call(dump),
        Instruction::LocalGet(0),
        Instruction::Call(proc_exit),
        Instruction::End,
    ] {
        function.instruction(&instruction);
    }
    function
}

fn begin_marker() -> Vec<u8> {
    format!("{BEGIN_MARKER}\n").into_bytes()
}

fn end_marker() -> Vec<u8> {
    format!("{END_MARKER}\n").into_bytes()
}

/// Emits `text` at the buffer pointer and advances it, without a data
/// segment: adding one would mean renumbering the module's own segments
/// and its data count, and the two markers are a few dozen bytes.
fn write_literal(function: &mut Function, pointer: u32, text: &[u8]) {
    for (chunk_index, chunk) in text.chunks(8).enumerate() {
        let offset = u64::try_from(chunk_index * 8).expect("a marker is a few dozen bytes");
        if let Ok(eight) = <[u8; 8]>::try_from(chunk) {
            function.instruction(&Instruction::LocalGet(pointer));
            function.instruction(&Instruction::I64Const(i64::from_le_bytes(eight)));
            function.instruction(&Instruction::I64Store(mem_i8(offset)));
        } else {
            for (byte_index, byte) in chunk.iter().enumerate() {
                function.instruction(&Instruction::LocalGet(pointer));
                function.instruction(&Instruction::I32Const((*byte).into()));
                let byte_offset =
                    offset + u64::try_from(byte_index).expect("a marker is a few dozen bytes");
                function.instruction(&Instruction::I32Store8(mem_i8(byte_offset)));
            }
        }
    }
    function.instruction(&Instruction::LocalGet(pointer));
    function.instruction(&Instruction::I32Const(as_i32(
        u32::try_from(text.len()).expect("the markers are a few dozen bytes"),
    )));
    function.instruction(&Instruction::I32Add);
    function.instruction(&Instruction::LocalSet(pointer));
}
