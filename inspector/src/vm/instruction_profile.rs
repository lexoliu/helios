//! Writing the guest's instruction-address samples as a folded profile.
//!
//! The kernel samples the instruction each processor's periodic timer
//! interrupted and names the wasm ones itself, because only it knows
//! which programs were loaded where. Everything else it hands back as a
//! bare address: the kernel image carries no symbol table at runtime,
//! and this host holds the exact ELF it booted. So the folded file is
//! assembled here, with the native addresses named against that image
//! at the base the guest reports it was loaded at.
//!
//! Each line is one location and its sample count: `wasm;<program>;<function>`,
//! `wasm;<program>;[trampoline]`, `kernel;<outermost>;…;<innermost>`, or
//! `unknown` for an address in neither. A kernel location is the chain of
//! functions inlined at the address, read from the image's DWARF: the
//! function whose symbol contains it first, the one the instruction
//! belongs to last. Where no DWARF function covers the address, the chain
//! is the containing symbol alone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use askama::Template as _;
use helios_inspector_protocol::system::profiling::{
    self as system_profiling, InstructionProfile, InstructionProfileError, SampledCode,
};

use super::kernel_image::{
    KernelDebugInfo, KernelSymbols, KernelSymbolsError, LoadedKernelSymbols, parse_image,
    read_image,
};
use super::{FoldedProfileLine, FoldedProfileTemplate};
use crate::serial::RpcClient;

/// Why the instruction profile a run asked for was not written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InstructionProfileOutputError {
    #[error(
        "failed to {} the guest's instruction sampling: {source}",
        if *.enabled { "start" } else { "stop" }
    )]
    Control {
        enabled: bool,
        #[source]
        source: helios_inspector_protocol::RpcError,
    },
    #[error("failed to read the guest's instruction samples: {source}")]
    Read {
        #[source]
        source: helios_inspector_protocol::RpcError,
    },
    #[error("the guest kernel cannot produce an instruction profile: {}", refusal_reason(*.refusal))]
    Refused { refusal: InstructionProfileError },
    #[error("cannot name the kernel addresses of the instruction profile: {source}")]
    Symbols {
        #[source]
        source: KernelSymbolsError,
    },
    #[error("failed to write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to report the written instruction profile: {source}")]
    Report {
        #[source]
        source: std::io::Error,
    },
}

fn refusal_reason(refusal: InstructionProfileError) -> &'static str {
    match refusal {
        InstructionProfileError::NotCaptured => {
            "this architecture's backend does not sample instruction addresses (only x86-64 does)"
        }
    }
}

/// Starts the guest's instruction sampling for the workload that follows.
///
/// Sampling is its own control, separate from the scoped profiler, because
/// it shortens every guest processor's timer period and so perturbs the
/// timings it runs under: only a run that asked for an instruction profile
/// starts it. Starting discards the samples of any earlier run.
pub(crate) async fn start(client: &RpcClient) -> Result<(), InstructionProfileOutputError> {
    set_sampling(client, true).await
}

/// Stops the guest's instruction sampling, once the workload is done and
/// before its samples are read.
pub(crate) async fn stop(client: &RpcClient) -> Result<(), InstructionProfileOutputError> {
    set_sampling(client, false).await
}

async fn set_sampling(
    client: &RpcClient,
    enabled: bool,
) -> Result<(), InstructionProfileOutputError> {
    system_profiling::set_instruction_sampling(client, enabled)
        .await
        .map_err(|source| InstructionProfileOutputError::Control { enabled, source })?
        .map_err(|refusal| InstructionProfileOutputError::Refused { refusal })
}

/// Reads the guest's instruction samples, taken between [`start`] and
/// [`stop`], and writes them to `output`, naming native addresses against
/// the kernel image at `kernel`.
pub(crate) async fn collect(
    client: &RpcClient,
    kernel: &Path,
    output: &Path,
) -> Result<(), InstructionProfileOutputError> {
    let profile = system_profiling::instruction_samples(client)
        .await
        .map_err(|source| InstructionProfileOutputError::Read { source })?
        .map_err(|refusal| InstructionProfileOutputError::Refused { refusal })?;
    let kernel = kernel.to_path_buf();
    let output = output.to_path_buf();
    // The kernel image is tens of megabytes; reading and indexing it
    // does not belong on the executor thread.
    blocking::unblock(move || write(&profile, &kernel, &output)).await
}

fn write(
    profile: &InstructionProfile,
    kernel: &Path,
    output: &PathBuf,
) -> Result<(), InstructionProfileOutputError> {
    use std::io::Write as _;

    let symbols = |source| InstructionProfileOutputError::Symbols { source };
    let bytes = read_image(kernel).map_err(symbols)?;
    let image = parse_image(kernel, &bytes).map_err(symbols)?;
    let table = KernelSymbols::from_image(kernel, &image).map_err(symbols)?;
    let debug = KernelDebugInfo::from_image(kernel, &image).map_err(symbols)?;
    let folded = render(
        profile,
        &KernelNames {
            symbols: table.loaded_at(profile.kernel_image_base),
            debug: &debug,
        },
    )
    .map_err(symbols)?;
    std::fs::write(output, folded).map_err(|source| InstructionProfileOutputError::Write {
        path: output.display().to_string(),
        source,
    })?;
    let samples = profile
        .samples
        .iter()
        .map(|sample| sample.count)
        .sum::<u64>();
    let mut stderr = std::io::stderr().lock();
    writeln!(
        stderr,
        "instruction_profile_output={} samples={samples} dropped={} period_ns={}",
        output.display(),
        profile.dropped,
        profile.period_nanos,
    )
    .map_err(|source| InstructionProfileOutputError::Report { source })
}

/// What the kernel's addresses are named with: its symbols, at the base
/// the guest reported, and its DWARF.
struct KernelNames<'table> {
    symbols: LoadedKernelSymbols<'table>,
    debug: &'table KernelDebugInfo,
}

/// The folded text of `profile`, heaviest location first.
fn render(
    profile: &InstructionProfile,
    kernel: &KernelNames<'_>,
) -> Result<String, KernelSymbolsError> {
    let mut stacks = BTreeMap::<String, u64>::new();
    for sample in &profile.samples {
        *stacks.entry(stack(&sample.code, kernel)?).or_insert(0) += sample.count;
    }
    let mut lines = stacks
        .into_iter()
        .map(|(stack, weight)| FoldedProfileLine { stack, weight })
        .collect::<Vec<_>>();
    lines.sort_by(|left, right| {
        right
            .weight
            .cmp(&left.weight)
            .then_with(|| left.stack.cmp(&right.stack))
    });
    Ok(FoldedProfileTemplate { lines }
        .render()
        .expect("folded profile template rendering is infallible"))
}

fn stack(code: &SampledCode, kernel: &KernelNames<'_>) -> Result<String, KernelSymbolsError> {
    Ok(match code {
        SampledCode::Wasm(function) => {
            let name = match &function.name {
                // A Rust program's name section may carry mangled
                // symbols; anything else demangles to itself.
                Some(name) => frame(&format!("{:#}", rustc_demangle::demangle(name))),
                None => format!("func[{}:{}]", function.static_module, function.index),
            };
            format!("wasm;{};{name}", frame(&function.module))
        }
        SampledCode::WasmTrampoline(module) => format!("wasm;{};[trampoline]", frame(module)),
        SampledCode::Native(address) if kernel.symbols.contains_text(*address) => {
            let chain = kernel
                .debug
                .inline_chain(kernel.symbols.link_address(*address))?;
            if chain.is_empty() {
                match kernel.symbols.symbolize(*address) {
                    Some(symbol) => format!("kernel;{}", frame(&symbol.function)),
                    None => "kernel;[unsymbolized]".to_owned(),
                }
            } else {
                let frames = chain
                    .iter()
                    .map(|function| frame(function))
                    .collect::<Vec<_>>();
                format!("kernel;{}", frames.join(";"))
            }
        }
        SampledCode::Native(_) => "unknown".to_owned(),
    })
}

/// One frame of a folded stack: `;` separates frames and a line ends the
/// sample, so neither may appear inside one.
fn frame(name: &str) -> String {
    name.replace([';', '\n', '\r'], "_")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use gimli::write::{Address, AttributeValue, DwarfUnit, EndianVec, Sections, UnitEntryId};
    use gimli::{Encoding, Format, LittleEndian, RunTimeEndian};
    use helios_inspector_protocol::system::profiling::{
        InstructionProfile, InstructionSample, SampledCode, WasmFunction,
    };

    use super::{KernelNames, render};
    use crate::vm::kernel_image::{DwarfReader, Function, KernelDebugInfo, KernelSymbols};

    const PARK: u64 = 0xffff_ffff_8000_1000;

    /// A subprogram DIE under `parent` with the given names.
    fn function(
        dwarf: &mut DwarfUnit,
        parent: UnitEntryId,
        linkage_name: Option<&str>,
        name: &str,
    ) -> UnitEntryId {
        let id = dwarf.unit.add(parent, gimli::DW_TAG_subprogram);
        let entry = dwarf.unit.get_mut(id);
        if let Some(linkage_name) = linkage_name {
            entry.set(
                gimli::DW_AT_linkage_name,
                AttributeValue::String(linkage_name.as_bytes().to_vec()),
            );
        }
        entry.set(
            gimli::DW_AT_name,
            AttributeValue::String(name.as_bytes().to_vec()),
        );
        id
    }

    /// Gives the DIE `id` the code range `start..start + length`.
    fn code_range(dwarf: &mut DwarfUnit, id: UnitEntryId, start: u64, length: u64) {
        let entry = dwarf.unit.get_mut(id);
        entry.set(
            gimli::DW_AT_low_pc,
            AttributeValue::Address(Address::Constant(start)),
        );
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(length));
    }

    /// An inlined call of `origin` under `parent`, over
    /// `start..start + length`.
    fn inlined(
        dwarf: &mut DwarfUnit,
        parent: UnitEntryId,
        origin: UnitEntryId,
        start: u64,
        length: u64,
    ) -> UnitEntryId {
        let id = dwarf.unit.add(parent, gimli::DW_TAG_inlined_subroutine);
        dwarf.unit.get_mut(id).set(
            gimli::DW_AT_abstract_origin,
            AttributeValue::UnitRef(origin),
        );
        code_range(dwarf, id, start, length);
        id
    }

    /// The DWARF an optimised build gives `park`: `wait` inlined over its
    /// first eight bytes, and `relax`, which carries only its short name,
    /// inlined into `wait` over two of them. `rdtsc_now`, past `park`, has
    /// a symbol and no DWARF, as code assembled without debuginfo does.
    fn debug_info() -> KernelDebugInfo {
        let mut dwarf = DwarfUnit::new(Encoding {
            format: Format::Dwarf32,
            version: 4,
            address_size: 8,
        });
        let root = dwarf.unit.root();
        dwarf.unit.get_mut(root).set(
            gimli::DW_AT_language,
            AttributeValue::Language(gimli::DW_LANG_Rust),
        );
        code_range(&mut dwarf, root, PARK, 0x100);
        let wait = function(
            &mut dwarf,
            root,
            Some("_RNvNtCs1234_13helios_kernel4exec4wait"),
            "wait",
        );
        let relax = function(&mut dwarf, root, None, "relax");
        let park = function(
            &mut dwarf,
            root,
            Some("_RNvNtCs1234_13helios_kernel4exec4park"),
            "park",
        );
        code_range(&mut dwarf, park, PARK, 0x100);
        let waiting = inlined(&mut dwarf, park, wait, PARK, 0x8);
        inlined(&mut dwarf, waiting, relax, PARK + 0x4, 0x2);
        let mut sections = Sections::new(EndianVec::new(LittleEndian));
        dwarf
            .write(&mut sections)
            .expect("the test DWARF is well formed");
        let dwarf = gimli::Dwarf::load(|section| {
            let bytes: Arc<[u8]> = match sections.get(section) {
                Some(data) => Arc::from(data.slice()),
                None => Arc::from([]),
            };
            Ok::<_, gimli::Error>(DwarfReader::new(bytes, RunTimeEndian::Little))
        })
        .expect("loading in-memory sections cannot fail");
        KernelDebugInfo::from_dwarf(Path::new("test-kernel"), dwarf).expect("the test DWARF parses")
    }

    #[test]
    fn samples_fold_into_wasm_frames_kernel_inline_chains_and_unknown() {
        let text = PARK..0xffff_ffff_8000_2000;
        let symbols = KernelSymbols::new(
            0xffff_ffff_8000_0000,
            vec![text],
            vec![
                Function {
                    name: "_RNvNtCs1234_13helios_kernel4exec4park".to_owned(),
                    address: PARK,
                    size: 0x100,
                },
                Function {
                    name: "rdtsc_now".to_owned(),
                    address: 0xffff_ffff_8000_1100,
                    size: 0x10,
                },
            ],
        );
        let debug = debug_info();
        let function = |name: Option<&str>, index| {
            SampledCode::Wasm(WasmFunction {
                module: "python3;bad".to_owned(),
                static_module: 1,
                index,
                name: name.map(str::to_owned),
            })
        };
        let profile = InstructionProfile {
            period_nanos: 1_000_000,
            kernel_image_base: 0xffff_ffff_8120_0000,
            samples: vec![
                InstructionSample {
                    code: function(Some("_PyEval_EvalFrameDefault"), 7),
                    count: 40,
                },
                // Inside `relax`, inlined into `wait`, inlined into `park`.
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1004),
                    count: 12,
                },
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1005),
                    count: 4,
                },
                // Inside `wait`, past `relax`.
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1006),
                    count: 6,
                },
                // `park`'s own code, one past the end of `wait`.
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1008),
                    count: 3,
                },
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1104),
                    count: 5,
                },
                // Text, but in no symbol and no DWARF function.
                InstructionSample {
                    code: SampledCode::Native(0xffff_ffff_8120_1800),
                    count: 1,
                },
                InstructionSample {
                    code: function(None, 9),
                    count: 2,
                },
                InstructionSample {
                    code: SampledCode::WasmTrampoline("python3;bad".to_owned()),
                    count: 2,
                },
                InstructionSample {
                    code: SampledCode::Native(0x4000),
                    count: 1,
                },
            ],
            dropped: 0,
        };

        let folded = render(
            &profile,
            &KernelNames {
                symbols: symbols.loaded_at(profile.kernel_image_base),
                debug: &debug,
            },
        )
        .expect("every address resolves against the test DWARF");

        assert_eq!(
            folded,
            "wasm;python3_bad;_PyEval_EvalFrameDefault 40\n\
             kernel;helios_kernel::exec::park;helios_kernel::exec::wait;relax 16\n\
             kernel;helios_kernel::exec::park;helios_kernel::exec::wait 6\n\
             kernel;rdtsc_now 5\n\
             kernel;helios_kernel::exec::park 3\n\
             wasm;python3_bad;[trampoline] 2\n\
             wasm;python3_bad;func[1:9] 2\n\
             kernel;[unsymbolized] 1\n\
             unknown 1\n"
        );
    }
}
