//! Finding the core module to work on, whether the input is a core module
//! or a component, and putting a rewritten one back.
//!
//! `python3.wasm` and `curl.wasm` are components; `qjs.wasm` is a core
//! module. Both shapes reach the compiler plugin as the same thing — a core
//! module Cranelift compiles — so both are hinted the same way, and the
//! component's other sections are copied byte for byte.

use std::ops::Range;

use wasmparser::{Parser, Payload};

use crate::{Error, module::CoreModule, wasm};

/// The core-module section id inside a component.
const COMPONENT_CORE_MODULE_SECTION: u8 = 1;

/// Where the core module this tool works on lives inside the input file.
#[derive(Debug, Clone)]
pub struct Location {
    /// Index of the core module among the component's top-level core
    /// modules; always 0 for a core-module input.
    pub core_module_index: u32,
    /// The core module's own bytes.
    pub body: Range<usize>,
    /// The bytes to replace when the module is rewritten: the whole
    /// component section, or the whole file for a core-module input.
    pub section: Range<usize>,
    pub is_component: bool,
}

/// Finds the core module to instrument or hint.
///
/// For a component that is the single top-level core module importing
/// `wasi_snapshot_preview1.fd_write`: the guest program itself, as opposed
/// to the preview1 adapter that serves it and the shim modules that wire
/// them together. Anything else is ambiguous and fails rather than
/// guessing.
pub fn locate(bytes: &[u8]) -> Result<Location, Error> {
    if bytes.len() < 8 || bytes[..4] != *b"\0asm" {
        return Err(Error::NotWasm("input".to_owned()));
    }
    // Byte 6 of the header distinguishes a component (0x01) from a core
    // module (0x00) in every version wasm-tools writes.
    if bytes[6] == 0 {
        return Ok(Location {
            core_module_index: 0,
            body: 0..bytes.len(),
            section: 0..bytes.len(),
            is_component: false,
        });
    }

    let mut candidates = Vec::new();
    for (index, body) in top_level_core_modules(bytes)?.into_iter().enumerate() {
        let module = CoreModule::parse(&bytes[body.clone()])?;
        if module.fd_write.is_some() {
            let index = u32::try_from(index).expect("a component holds fewer than 4G modules");
            candidates.push((index, body));
        }
    }

    let mut candidates = candidates.into_iter();
    let (core_module_index, body) = match (candidates.next(), candidates.next()) {
        (Some(only), None) => only,
        (first, second) => {
            let found = usize::from(first.is_some()) + usize::from(second.is_some());
            return Err(Error::CoreModuleAmbiguous { found });
        }
    };

    let header_start = wasm::header_start(bytes, COMPONENT_CORE_MODULE_SECTION, &body);
    Ok(Location {
        core_module_index,
        section: header_start..body.end,
        body,
        is_component: true,
    })
}

/// Replaces the located core module with `module`, leaving every other
/// byte of the input alone.
pub fn splice(bytes: &[u8], location: &Location, module: &[u8]) -> Vec<u8> {
    if !location.is_component {
        return module.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() + module.len());
    out.extend_from_slice(&bytes[..location.section.start]);
    out.extend(wasm::section(COMPONENT_CORE_MODULE_SECTION, module));
    out.extend_from_slice(&bytes[location.section.end..]);
    out
}

/// Describes the located module for a build log.
pub fn describe(location: &Location) -> String {
    if location.is_component {
        format!(
            "core module {} of the component",
            location.core_module_index
        )
    } else {
        "the core module".to_owned()
    }
}

fn top_level_core_modules(bytes: &[u8]) -> Result<Vec<Range<usize>>, Error> {
    let mut modules = Vec::new();
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(Error::parse("component"))? {
            Payload::ModuleSection {
                unchecked_range, ..
            } => {
                if depth == 0 {
                    modules.push(unchecked_range);
                }
                depth += 1;
            }
            Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    if modules.is_empty() {
        return Err(Error::CoreModuleAmbiguous { found: 0 });
    }
    Ok(modules)
}

/// Reads the located core module out of `bytes`.
pub fn core_module<'a>(bytes: &'a [u8], location: &Location) -> Result<CoreModule<'a>, Error> {
    CoreModule::parse(&bytes[location.body.clone()])
}
